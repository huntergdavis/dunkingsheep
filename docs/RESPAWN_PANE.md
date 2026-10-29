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
- missing `--confirm-idle`;
- active declared turn;
- live task lease for the pane;
- unread mail or unknown unread count;
- any queued outbox message for the pane;
- delivery state other than `ready`, or foreign text in the composer;
- no rollback argv (explicit or saved launch argv);
- backend without guarded respawn support.

These are checked before a process is touched. Task/mail stores are not written.

## Success and rollback

On success the old launch record is preserved under `previous`, the new argv is
recorded with `source="respawn_pane"`, and one `pane.respawn` metric is written.
The replacement pid is verified live before success is returned.

If the backend fails after the old process is gone, or the replacement is not a
live new pid, the saved rollback argv is launched in the same pane, a
`pane.respawn.rollback` metric is written, and the command raises. A failed
attempt never replaces the old launch record. If rollback itself fails, the
error says so; no success is claimed.

## Tests

`python3 -m unittest tests.test_dunk_respawn_pane` covers success (old launch
preserved), wrong pid/start, missing idle confirmation, active turn, unread,
outbox, live lease, non-ready delivery, unsupported backend, failed start with
rollback, dead replacement with rollback, stale-pid second attempt, and
backend argv quoting/unmanaged refusal. No live pane is used.
