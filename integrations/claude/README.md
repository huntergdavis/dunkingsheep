# Claude turn producer

A generation-aware turn hook for Claude Code lanes: `UserPromptSubmit` opens a
daemon turn and captures its token; `Stop` closes it with that token;
`SubagentStop` is wired to `noop` so a finishing subagent can never close the
parent turn.

## Why not the installed `~/.claude/hooks/dunkingsheep-turn.sh`

That script is tokenless: start calls the CLI without `--json` and discards
the returned turn id, Stop calls turn end without `--turn-id`, and SubagentStop
shares the same end. The daemon's authenticated `turn_end` requires the matching
token plus a stable caller, so nothing the old script does can authenticate.
This producer captures the token on start, consumes it before the end RPC
(a replay can never send it twice), and binds it to the Claude session plus
the starter's pid/start time.

## Wiring (fixed)

| Claude event     | action | effect                                              |
|------------------|--------|-----------------------------------------------------|
| UserPromptSubmit | start  | open turn, capture `--json` token to 0600 state     |
| Stop             | end    | close with the token; exact `hook_event_name` only  |
| SubagentStop     | noop   | log and exit 0; never touches parent turn state     |

Two independent barriers keep subagents from ending the parent turn: the
installer wiring above, and `end()` requiring the exact hook event name
`"Stop"` (per the official hooks reference, a SubagentStop payload carries
`stop_hook_active` too, so shape alone cannot tell them apart).

Claude exposes no per-turn event identity: there is no turn id in any hook
payload.  Correlation is (session id, starter pid/start, at most one open
turn) under one documented ordering assumption: Claude runs a single agent
loop per session, and a Stop hook can veto the stop, so the next
UserPromptSubmit cannot arrive before the prior turn's Stop chain completes.
Concurrent duplicate Stop callbacks are serialized by the state lock with
consume-before-RPC: exactly one wins and only the winner performs the end
RPC (proven by a deterministic barrier test with two racing ends).  The
duplicate keeps the existing token (starting again would orphan the daemon
turn to its TTL). A replaced pane
root starts fresh (logged); a stale/expired token is refused and cleaned up;
an end with no start token is skipped, never invented.

The pane is never supplied as a label: the CLI resolves the caller's pane
from the terminal it runs in, and the daemon re-checks kernel ancestry
(`require_turn_caller`). No pane identity here is trusted on its own.

## Install (test lanes only, after independent review)

```sh
python3 integrations/claude/install.py --home /tmp/claude-test-home
python3 integrations/claude/install.py --home /tmp/claude-test-home --check
```

Merges the three hook entries into that home's `settings.json` (unrelated
hooks preserved, backup written, already-wired installs left byte-for-byte).
Never point `--home` at a production `~/.claude` until this producer is
independently accepted and its rollout is coordinated.

State: `~/.local/state/dunkingsheep/claude-turns/` (`DUNKINGSHEEP_CLAUDE_STATE_DIR`
overrides), directory `0700` (chmodded back even if pre-created wider),
files `0600` created before any secret bytes exist (`os.open`, never an
umask-dependent open-then-chmod). Logs carry no
tokens or prompt text. Every hook path exits 0: a nonzero UserPromptSubmit
would block the agent's prompt.
