# Agent-state capture: the record a restart would need

The owner asked for every agent to be restartable by the VP with its previous state —
project, agent binary, model, thinking level, session — recorded continuously by this
daemon, deterministically, no model. This is the **capture** half. The restore is
deliberately not built yet: it is gated on the VP's scope.

## What is recorded

One row per agent lane, on every `start-agent` and every five minutes
(`dunkingsheep-state.timer`), in `agent-state.db`:

| field | source | when it cannot be read |
|---|---|---|
| project | the pane's cwd | `null` |
| harness | the pane record | `null` |
| cwd, argv | `launches.json` (the exact argv that started the pane) | `null` |
| session | `launches.json` (`session`/`session_kind`), claude's `sessionId` | `null` |
| model, thinking | the launching argv (`--model`, `--thinking`/`--reasoning`) | **null + "the launching argv carries no model flag"** |
| state, state_source, state_reason | `dunk_activity` (per harness; unknown when absent) | `unknown` + reason |
| task_id | the lane's live lease | `null` |

**A null is never a default.** A wrong model in a restore looks like a decision, which
is worse than an admitted gap, so every unreadable field carries a reason string in
the same row.

## Why the capture can run tonight and the restore cannot

Capture is additive: it writes a new table, touches no live pane, and deleting the DB
loses nothing but history. The restore changes which terminal backend runs agents, so
it waits for the VP's scope confirmation — the CoS drew that line explicitly and this
note records it, because "we started it" is not permission for the irreversible half.

## What a restore would need (three tiers, decided before it is built)

1. **Exact resume** — every harness supports it (verified from each binary's own
   `--help`): `claude --resume <session-id>`, `pi --resume/--session <id>`,
   `codex resume --last`. Needs the session file to still exist and the same cwd.
2. **Fresh session + handoff** — start the same harness in the same cwd with a one-line
   "you were working on X; last state: <row>" (what `recover` already does for bare
   shells).
3. **Task re-claim** — resume the *work*, not the conversation: the task board is the
   durable unit, and the row records which task the lane held.
