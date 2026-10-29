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
| Stop             | end    | close with the token; Stop-shaped events only       |
| SubagentStop     | noop   | log and exit 0; never touches parent turn state     |

Two independent barriers keep subagents from ending the parent turn: the
installer wiring above, and `end()` refusing any event without Claude's
`stop_hook_active` key (which SubagentStop-shaped events do not carry).

Claude exposes no turn id, so one session holds at most one open turn: a
duplicate `UserPromptSubmit` for a live session keeps the existing token
(starting again would orphan the daemon turn to its TTL). A replaced pane
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
overrides), directory `0700`, files `0600`, `fcntl`-locked. Logs carry no
tokens or prompt text. Every hook path exits 0: a nonzero UserPromptSubmit
would block the agent's prompt.
