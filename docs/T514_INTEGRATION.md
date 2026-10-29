# t514: integrated deterministic allocation and self-recovery tree

Owner #2389 asked for a Dunking Sheep operating harness that assigns ready
backlog and reviews, and performs bounded self-recovery, with **deterministic
rules and no model deciding any job**. Three accepted private changes existed
on three branches; the owner-delivery fixes PR113/PR114 had since landed on
main. This document records the exact integrated tree, what was changed to make
the pieces coexist, the evidence, the limits, and the rollout and acceptance
plan. Nothing here was run against production: no daemon restart, no config,
no pane input, no registration, no merge.

## Integrated tree

Branch `claude/deterministic-integration`, worktree
`/home/hunter/workspace/ds-claude-integration`, base `origin/main` 182e602
(PR114 merge, release 2.70.102).

| Commit | What | Source |
|---|---|---|
| 29d9044 | merge PR110 head `635268ded9` | pid + start-ticks identity for state capture (t505/t500) |
| 7e9aa53 | merge PR111 head `e3be6d558b` | deterministic registered-worker allocator (t507, reviewed in t509) |
| 3df50a1 | merge PR112 head `337e0c6467` | native-bound owned-prompt recovery executor (t508, accepted in t511) |
| 4400a6e | integration fix + regressions | federated and legacy stuck observers partitioned by frame ownership |
| 36def5d | release 2.70.103 | version bump only |
| 554577e | GUIDE.md regenerated; `tools/t514_rollout.sh` | guide was stale at 2.70.100 on main |

All three PR heads are exactly the pinned SHAs from the task body; no source of
theirs was rewritten. PR110 and PR111 merged clean. PR112 had one textual
conflict in `Flock.submit_text` (main captures the Codex busy hint before
typing; PR112 binds the durable attempt generation before typing). Both are
pre-typing observations under the same mutex hold and both were kept.

## The one semantic conflict, and its fix (commit 4400a6e)

Production runs with `DUNKINGSHEEP_FEDERATED_INPUT=1` (drop-in
`30-reviewed-release.conf`). In that mode main's `_stuck_observer_step` runs
PR113's `_federated_recovery_step` first, then the legacy loop, and the legacy
loop skips any pane whose frame the federated parser recognises ("one observer
per pane"). PR112's native-bound recovery lives inside that legacy loop.

The federated step, however, evaluated every stuck candidate pane, and for a
frame it did **not** recognise it called `dunk_stuck.decide` with empty text.
The tracker keys its records by pane, so that popped the legacy record every
5 s tick, and the legacy clock restarted forever. Integrated and deployed as-is,
PR112's recovery of the captured 628-character CoS frame would never fire in
production. The fix makes the federated step skip frames it does not own, the
mirror image of the legacy loop's existing skip. Exactly one observer and at
most one key per pane in either direction.

`tests/test_dunk_integration_t514.py` (5 tests, built on PR112's private
fixtures) pins: plain captured frame in federated mode recovers once via the
native path (**fails before the fix, passes after**); the same frame styled the
way Codex draws it recovers once via the federated actor with the native path
never engaging; strict mode unchanged; `_recovery_box_text` reader order; and
the merged `submit_text` block binds the attempt generation and the busy hint
before `send_text`.

## Test evidence (this tree, hermetic, heavy-job semaphore ticket 1282)

| Run | Result |
|---|---|
| focused: allocator, tasks, task_pool, next_job x3, recovery, recovery_wiring, state_capture_cost, federated_recovery, federated_backoff, busy_native_queue, dialog_alerts, delivery, delivery_backoff, stuck_recovery, scheduled_recovery, codex_ambiguous_composer, submit_honesty, guarded_nudge_submits, state_session, native_session, dispatch, cli | 435 tests, 3 failures, all in `test_cli` (see below) |
| seam suites after the fix: integration_t514, federated_recovery, federated_backoff, recovery_wiring, stuck_recovery, scheduled_recovery, busy_native_queue, dialog_alerts | 164 tests OK |
| red control: fix reverted, `test_dunk_integration_t514` | 1 failure (the plain-frame test), as intended |
| full `python3 -m unittest discover -s tests` | see the task note for the final count |

`test_cli` failures: `test_commands_listing_and_version` was the version bump
landing mid-run (CLI subprocess read 2.70.103, test imported 2.70.102); it
passes on the committed tree. `test_self_discovery_surfaces` failed because
`docs/GUIDE.md` was stale on main; regenerated here. `test_full_lifecycle_through_the_cli`
(`m1 sent` expected, `m2 sent` found) **fails identically on unmodified
origin/main 182e602** in a scratch worktree, so it is a pre-existing main defect
outside the three PRs and is reported, not fixed here. Every suite emits the
usual ResourceWarnings.

`git diff --check` reports one new blank line at EOF in
`tests/fixtures/t508-cos-ambiguous.txt`; that file is a verbatim capture and was
left byte-identical to PR112.

## What is deterministic, and where the switches are

- Allocation: `Allocator.tick` in the task watchdog loop, at most every 30 s.
  An empty registry means **off**. Gates in order: dispatch kill switch and
  lane opt-in (`NextJobs.gate`, registration counts as opt-in), authenticated
  ended turn, no pending delivery, caller pane auth, zero unread, guarded
  delivery state ready; then one SQLite transaction claims, transfers
  ownership and writes the `nj-*` next-job intent. Ordering: priority, oldest
  created, task id; workers by oldest last allocation then pane id; capacity
  one. CLI `dunkingsheep allocation {status,register,offer,review}` requires
  superuser for mutations.
- Recovery: the existing `DUNKINGSHEEP_STUCK_CONFIG` (production: threshold
  120 s, max 1, cooldown 60 s, observe 20 s). PR112's native path additionally
  requires an exact durable attempt, unchanged generation, process-bound native
  session, prompt absent from the transcript, accounted preceding rows, idle
  lane, no dialog; one Enter per message/creation/generation even if the legacy
  budget were larger. Bare legacy Codex (the live CoS) refuses unless
  `DUNKINGSHEEP_RECOVERY_BINDINGS` names an independently verified
  pid/start/cwd/home/session.
- Nothing in either path consults a model.

## Staged rollout and rollback

`tools/t514_rollout.sh` is dry-run by default and prints every command; only
the runtime operator VP15 designates runs `--apply`. It matches the live shape:
immutable release trees under `~/.local/share/dunkingsheep/releases/<sha>`, the
launcher `~/.local/bin/dunkingsheep`, and `dunkingsheep.service` with ExecStart
pinned in a drop-in.

1. **Stage** (`stage --sha <head> --apply`): export the reviewed SHA into a new
   release directory, compile, print `--version`. No restart, nothing live
   changes.
2. **Activate** (`activate --sha <head> --apply`): write
   `40-t514-integrated-release.conf` overriding ExecStart only (the federated
   and stuck-config environment in `30-reviewed-release.conf` is untouched),
   reinstall the launcher from the new tree, `daemon-reload`, restart. The
   restart interrupts queued deliveries for a few seconds; the operator picks
   the moment (no human-active pane mid-turn, no lane restart).
3. **Verify** (`verify`): service active, launcher path, `status` shows
   2.70.103, `guard check`, `allocation status --json` shows an empty registry
   (allocator off), daemon log tail.
4. **Rollback** (`rollback --apply`): remove the override drop-in (ExecStart
   returns to 182e602, already staged), reinstall that launcher, restart.
   Rollback to any other staged sha: `rollback --sha <sha> --apply`.

Stage 2 and 3 of the *feature* rollout are operator CLI actions, not code:
registering workers (allocation on), and, only after independent verification
of the binding, setting `DUNKINGSHEEP_RECOVERY_BINDINGS` for the bare CoS.

## Acceptance plan for real automatic assignment (no synthetic throughput)

Each clause names the read-only evidence a third party checks. A manual nudge,
a hand-typed prompt or a label never counts.

1. **Native start, worker.** Register one real idle worker lane in a `canary`
   pool (`allocation register --pane-id %W --pools canary --roles worker`).
   Offer one real, bodied, owned low-risk task (`allocation offer --task-id tN
   --pool canary`). Within one tick after the lane's next authenticated ended
   turn: `task_events` has `allocation_offered` then the `nj-*` handoff row with
   `trigger=allocation`; `task_leases` shows `%W`; the inbox row for the prompt
   has a delivery receipt; the lane's next turn (t363 binding) starts after
   that receipt. Chain: offer → allocation → lease → message id → turn start,
   each timestamped.
2. **Priority then age.** Offer two tasks of different priority, then two of
   the same priority created at different times; the allocation order recorded
   in `task_events` is priority first, then oldest `created_at`, then id.
3. **Non-author review.** The worker submits `allocation review --task-id tN
   --revision <sha>`; a review task is created once, assigned to a registered
   reviewer who is not an owner, creator or completer of tN; repeating the
   same revision returns the same review id (no duplicate); the author's
   `task next` cannot claim it.
4. **No duplicates under restart.** Restart the daemon (operator) between an
   offer and the next tick; exactly one lease and one handoff exist afterwards.
5. **Quiet lanes stay quiet.** Reviewer, CoS and VP panes with no registration
   and no assigned work show `workers[pane] = missing` or stay absent from
   `allocation status`; no prompt is delivered to them.
6. **Recovery canary.** On a registered Codex lane, after an injected failed
   submit is reproduced in a scratch workspace (never the owner's panes), the
   daemon log shows one `our own text sat unsubmitted ... attempt 1 of 1`
   line, one Enter, and `recovery_outcome=consumed` on the candidate, with
   the inbox notification repaired. For the bare CoS this clause is **blocked**
   until the binding is independently verified.

Live behaviour, the t372 acceptance run and owner + CoS sign-off remain open.
This tree and this document are source-level evidence only.
