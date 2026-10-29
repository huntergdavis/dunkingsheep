# Guarded same-pane respawn (t398)

`dunkingsheep respawn-pane` replaces the process in an existing managed pane
while preserving the pane id, task ownership/lease state, mailbox, allowlist and
the agent's native session. It is the missing activation primitive for Codex
lanes whose producer must run with `--no-daemon` in the pane ancestry.

It is **superuser/operator only**. The author does not run it during review; CoS
performs the actual activation.

## Command

```sh
dunkingsheep respawn-pane %31 \
  --expected-pid 1914154 --expected-start 1008931570 --confirm-idle \
  --reason "t395 codex producer activation" \
  --command-json '["env","CODEX_HOME=/home/hunter/workspace/t395-activation/%31/codex-home","DUNKINGSHEEP_CODEX_STATE_DIR=/home/hunter/workspace/t395-activation/%31/state","codex","--no-daemon","--dangerously-bypass-hook-trust","-C","/home/hunter/workspace/poolrad-codex-review-d","-s","danger-full-access","-a","never","resume","01a173ee-cf57-7603-a719-febcafc092be"]' \
  --rollback-json '["env","CODEX_HOME=/home/hunter/workspace/t395-activation/%31/codex-home","DUNKINGSHEEP_CODEX_STATE_DIR=/home/hunter/workspace/t395-activation/%31/state","codex","-C","/home/hunter/workspace/poolrad-codex-review-d","-s","danger-full-access","-a","never","resume","01a173ee-cf57-7603-a719-febcafc092be"]'
```

The command is an argv array, never a shell string. The backend joins it with
`shlex` and runs it through the existing visible-exit wrapper; caller text is not
interpreted as shell code. The per-lane `CODEX_HOME`/hooks are isolated; the
shared `~/.codex` hooks and helpers are not changed. Use the per-lane home
prepared from the accepted bundle with symlinks to the owner auth/session/state
(see `t395-codex-activation-31.md`); `--dangerously-bypass-hook-trust` applies
only to that reviewed per-lane hook file.

## Refusals (fail closed)

- no superuser allowlist entry / wrong operator;
- pane not visible or not created by Dunking Sheep;
- `--expected-pid`/`--expected-start` mismatch (including a replaced/reused pid);
- a FINAL identity re-check inside the backend immediately before the kill/spawn
  catches a pane replaced between the core checks and the mutation;
- an explicit shared `CODEX_HOME=~/.codex` on a Codex lane is refused; use the
  isolated per-lane home;
- missing `--confirm-idle`;
- active declared turn;
- live task lease for the pane;
- unread mail or unknown unread count;
- any queued outbox message for the pane;
- delivery state other than `ready`, or foreign text in the composer;
- no rollback argv (explicit or saved launch argv);
- backend without guarded respawn support;
- the replacement wrapper is alive but the intended agent is absent: the
  `_verify_agent_started` screen check and a descendant-process check must both
  find the harness, not just the wrapper pid.  The descendant walk requires a
  non-dead state (`DEAD_STATES`, plus stopped/traced `T`), a start time no older
  than the new wrapper (so a leftover child from the previous agent is not
  evidence), and an expected kernel `comm` (not an argv basename, so a wrapper
  script named `codex` is not evidence).  A pid that vanishes mid-walk is
  simply no evidence.

These are checked before a process is touched. Task/mail stores are not written.

## Native-session quiescence (t416)

For a Codex command that resumes a native session, pane idleness is **not**
session quiescence. An older shared app-server turn can still be running while
the pane is blank and no task lease exists (measured `%30`, 2026-10-29: old turn
`01a18644…` was still running when the replacement started `01a18645…`).

The guard resolves the incoming `CODEX_HOME`, the exact thread row in its
`state_5.sqlite`, and that row's exact rollout path. The first record must be a
`session_meta` naming this session and cwd, and the rollout must end at a record
boundary. It then refuses when any `task_started` has no matching
`task_complete`; the refusal names the unfinished turn id. A quiet session is
allowed only when its authenticated, complete trace has no active turn. Missing,
empty, malformed, truncated, duplicate, start-less-completion, or wrong-session
history is **not** treated as quiet.

The final check re-reads the rollout stat immediately before mutation and
refuses if it changed. **BOUND:** the re-read closes the window up to the read;
a turn starting after it is not prevented. This is a bounded precondition, not
atomic coordination with Codex's session writer.

Tests: `tests.test_dunk_native_session` covers the saved `%30` timeline cut at
06:07:44 (refuse naming the old turn), the older turn completed before respawn
(allow), authenticated quiet history (allow), empty/missing/truncated/wrong-
session evidence (refuse), malformed lines, cwd/path/session mismatch, and a
change after the snapshot (refuse). Respawn integration covers active-turn
refusal before any backend call and completed-turn allowance.

## Success and rollback

On success the old launch record is preserved under `previous`, the new argv is
recorded with `source="respawn_pane"`, and one `pane.respawn` metric is written.
The launch record is written only AFTER the replacement exists and is verified
live; a backend refusal before mutation leaves the old launch record untouched.
The replacement pid is verified live before success is returned.

If the backend fails after the old process is gone, or the replacement is not a
live new pid, the saved rollback argv is launched in the same pane, a
`pane.respawn.rollback` metric is written, and the command raises. A failed
attempt never replaces the old launch record. If rollback itself fails, the
error says so; no success is claimed.

## Tests

`python3 -m unittest tests.test_dunk_native_session tests.test_dunk_respawn_pane` covers the native guard controls and the existing respawn cases: success (old launch
preserved), wrong pid/start, missing idle confirmation, active turn, unread,
outbox, live lease, non-ready delivery, unsupported backend, failed start with
rollback, dead replacement with rollback, stale-pid second attempt, pane
replaced between core check and backend mutation, explicit shared CODEX_HOME,
backend refusal keeping the old launch record, a live wrapper with a failed
child (`command not found`/`[agent exited rc=127]`) rolling back, a live wrapper
with no intended child rolling back, direct helper controls for a zombie child,
stopped child, pre-respawn leftover child, live post-respawn child and a
vanishing-proc race, and backend argv quoting/unmanaged refusal. No live pane is
used.
