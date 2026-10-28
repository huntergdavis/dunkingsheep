# t374 acceptance: real automatic A-to-B work

This is the runbook for the only acceptance that closes t374: a **real pi agent**
and a **real Codex agent**, each on a dedicated lane, complete job A and then
receive, start, and complete job B **without a human Enter** and without a
synthetic turn event.

The tool is `tools/t374_acceptance.py`.  It prepares the jobs and launch
commands, collects the live store records, and validates the chain.  It never
presses a key, never calls `send_text`/`send_keys`, and never fabricates a turn.

## Dedicated producer bundle

The release verifier pins one bundle per run:

```sh
python3 tools/t374_bundle.py --out <run>/bundle --source-ref <ACCEPTED-PR72-head>
```

It writes `bundle/pi/extensions/dunkingsheep.ts` (installed extension, prior
patch reversed, PR72 patch applied, with `pi/PATCH-APPLICATION.txt` and a
SHA-256), `bundle/codex/` (PR72 helper/wrapper/hooks + the owner-default
`config.toml`; auth is linked at run time, never copied), and
`bundle/opencode/dunkingsheep-turn.js`.  `bundle/manifest.json` records the
source head and every file checksum.  The launch commands use the bundle paths
and no model override.  The bundle does not start agents and does not edit the
installed pi extension or `~/.codex`.

## What is prepared, and what is not

Prepared now:

- two bounded jobs per lane (`A` and `B`) with private artifacts under one run
  directory;
- exact launch commands for a dedicated pi lane and a dedicated
  `codex --no-daemon` lane with the t264 producer hook;
- a `prepare` step that requires the authentic **operator pane** and creates
  **global tasks exclusively assigned** to the dedicated pane.  The guarded CLI
  refuses cross-pane private-task creation (`per-pane tasks are private to
  <target>; caller runs in <operator>`), so a global task with the target as the
  sole assignee is the supported no-impersonation path.  It is not actionable by
  any other lane; the privacy trade is explicit and was reviewed as a delta from
  589e20c;
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
- A/B are global-scope but **exclusively assigned** to the dedicated pane.  If
  the integrated loop cannot see an assigned global task, that is a blocker to
  report and fix; do not weaken the run to unassigned or shared work.

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

3. Within that window, the authentic operator creates the global, exclusively
   assigned tasks.  The tasks must be created before the agent's `task next`
   runs; the `sleep 30` prompt is the window.  The operator lane is passed
   explicitly and must differ from both dedicated target panes:

   ```sh
   python3 tools/t374_acceptance.py prepare \
       --run <run-dir> --execute --approval <approval.json> \
       --operator-pane %2 --pi-pane <pi-pane> --codex-pane <codex-pane>
   ```

   It creates A and B for each pane with `--scope <pane> --assign <pane>` and
   prints the ids.  It does not launch or nudge anything.

4. Do **not** send any further prompt or key.  Each agent runs
   `dunkingsheep task next` once, claims its assigned A, writes
   `<run>/<lane>/A.artifact`, completes A.  The automatic handoff must claim B,
   deliver it as a `[turn-end ...]` prompt, start a new turn, and the agent must
   write `<run>/<lane>/B.artifact` and complete B.

5. Collect the live stores immediately after B completes (messages are recent):

   ```sh
   python3 tools/t374_acceptance.py collect --run <run-dir>
   python3 tools/t374_acceptance.py validate --receipt <run-dir>/receipt.json
   ```

6. The producer must echo the handoff id.  The B prompt contains
   `Handoff nj-... .`; the pi patch / Codex hook / OpenCode plugin from PR72
   extracts it and sends `turn start --handoff-id nj-...`.  A producer without
   that echo leaves the handoff at `sent`, and validation fails.  Do not fake
   the echo in the test harness.

7. Clean up with the guarded task CLI (the tool prints the commands):

   ```sh
   python3 tools/t374_acceptance.py cleanup --run <run-dir> --operator-pane %2
   python3 tools/t374_acceptance.py cleanup --run <run-dir> --operator-pane %2 --execute
   ```

## What the receipt must show, per lane

| stage | store / field |
|---|---|
| A claimed by the agent itself | `tasks.db` `task_events` `claimed`, `task_leases.caller_name` = `pi` or `codex` |
| A artifact | `<run>/<lane>/A.artifact` starts with `t374 A <run> <lane>` |
| A complete | `tasks.db` `task_events` `completed` |
| handoff | `tasks.db` `task_events` event `next_job_handoff` on B, meta `{from_task: A, message_id: <id>}` |
| B claim | one `task_events` `claimed` on B, after A complete and after the handoff |
| B delivery | `inbox.db` `nj-` row: `sender=next-job`, `notify_state=sent`, `notify_at`, `notify_transport_id`, `notify_attempt`; handoff `transport_id`/`attempt`/`delivered_at` agree |
| B consumption | the same inbox row has `read_at` or `acked_at` |
| B start | handoff `state=started`, `turn_id`, `turn_started_at > attempt_at` and `<= B completion`, `turn_caller.name` = lane; producer echoed `--handoff-id nj-...`; matching `metrics` `turn.start`; independent sent receipt still required |
| B complete | `tasks.db` `task_events` `completed` on B at/after the turn start; B artifact prefix |
| no human nudge | zero `send_text` messages to the lane during the run |
| no duplicate B | exactly one handoff message id, one B claim, one `nj-` receipt |
| no work after B | a `next_job_decision` record on B with `reason=no_assigned_work`; no C claimed or invented |

**Fast-turn ordering:** `delivered_at` is `message.finished_at`, set after
`submit_text` returns, and `notify_at` is later still.  A valid exact-token turn
can start after `attempt_at` and **before** `delivered_at`/`notify_at` (measured
on dd243b4: attempt_at < turn_started_at < delivered_at < notify_at for the same
`nj-` id and attempt).  The validator therefore requires
`turn_started_at > attempt_at` and `<= B completion`, not after the send
confirmation; the independent sent receipt and the bound turn id are still
required.

The validator also rejects B claimed before A completed, a handoff without a
`message_id`, a wrong `from_task`, a duplicate B claim/delivery, a turn before
the transport attempt, a turn only after B completion, a missing B turn start,
a missing artifact, and any human send.

## Negative probes

- **Repeated completion:** after A is complete, re-run
  `dunkingsheep --self-pane <pane> task complete <A>` once.  The chain must still
  show exactly one B handoff/claim/delivery; a second is a failure.
- **No-work:** after B completes, the same completion trigger records a
  `next_job_decision` with `reason=no_assigned_work`.  No C task or prompt may
  appear.
- **No-human-nudge:** the launch prompt is the only human input; the runbook
  forbids any `send`, `send-keys`, or Enter during A/B.  The receipt counts
  `send_text` messages to the lane and must show zero.

## Current state (2026-10-28)

Prepared only.  The existing bundle artifact is **PROVISIONAL**: it was generated
from `dd243b4`, which is now superseded (a later PR72 correction is pending
review).  Do not use it for a live run.  When the integrated release is accepted,
regenerate once with `--source-ref <accepted head>`; the producer files may be
byte-identical, but the manifest must name the accepted source.  PR71's corrected
head is also pending.  No agent has been started.  No live pi/Codex acceptance run has
been executed against an approved integrated release; no owner-home installation
or live-lane respawn has been done.  This runbook and the tool are the
preparation artifact.
