# Harness turn-hook Python boundary

The native `dunk-poll` release client removes Python from the poll launcher of a
new Rust release. It does not remove Python from the token-aware Claude and
Codex turn-hook producers or their installers. Existing installations and
rollback releases may use different, older hooks; installing a release does not
rewrite harness settings or update running lanes.

| Harness | Repository turn producer | Boundary to preserve |
| --- | --- | --- |
| Claude | `integrations/claude/dunkingsheep-turn.sh` invokes `turn-generation.py` | In-pane process ancestry, serialized session state, the daemon's start token, starter PID/start identity and exact Stop event. `SubagentStop` is `noop`; it must not end the parent turn. |
| Codex | `integrations/codex/dunkingsheep-turn.sh` invokes `turn-generation.py` | In-pane ancestry with `codex --no-daemon`, serialized session state, harness turn ID, daemon start token, and starter PID/start identity. Stale, replaced or replayed turns must not end a newer turn. |
| OpenCode | `integrations/opencode/dunkingsheep-turn.js` | Root-session identity, in-pane ancestry, daemon start token, and consume-before-end. The plugin runs in the OpenCode process; its installer is Python. |
| Pi | `integrations/pi/turn-generation.patch` | Serial start/end, preserve the daemon start token as `turn_id`, and send no end without a matching start. The patch targets the Pi extension, not the daemon. |

A native replacement for either Python producer must preserve its harness's
specific event identity, serialization and failure behavior. Hooks must not
block a user turn when the daemon refuses a request. Verify successful token
propagation and stale-generation refusal with isolated temporary sockets and
process identities, then verify the actual per-lane installation separately.
Do not infer the active hook from a default home: a lane can have a separate
`CODEX_HOME`, a different Claude configuration or an older wrapper. Do not
replace live hook files or trust hashes as part of a source-only build.

Until these producers and any required installers have native replacements or
an explicitly accepted external harness boundary, Python remains a possible
runtime dependency even when the daemon, desk and poll client are Rust. Test
oracles are a separate dependency described in [Python daemon retirement](PYTHON_DAEMON_RETIREMENT.md).
Older releases may still require Python for rollback. Preserve them until the
owner separately resolves the rollback policy.
