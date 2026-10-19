# Memory migration: agents off deja onto Dunking Sheep memory (t11)

Status: **cutover in progress**. Dunking Sheep memory is the primary recall path;
deja stays installed, answering, and **read-only** until the bounded parity is
signed off. Nothing was uninstalled, so every switch is reversible by editing one
file.

## What each agent now uses

| Agent | Was | Now | Fallback |
| --- | --- | --- | --- |
| pi (startup context) | `deja hook-context` in `~/.pi/agent/extensions/deja.ts` | `dunkingsheep memory hook-context --sessions 5`, injected as the hidden `memory-context` message | `deja hook-context` if memory is unreachable or empty |
| pi (tools) | deja MCP `recall` / `recall_context` (`direct`) | `dunkingsheep` MCP `memory_recall` / `memory_context` / `memory_search` / `memory_stats` / `memory_status` set to `toolExposure: direct` | deja MCP stays registered and `direct` |
| pi (instructions) | `~/.pi/agent/AGENTS.md` deja section | same file, memory section, deja named as read-only fallback | — |
| Claude Code (session start) | `~/.claude/settings.json` SessionStart → `deja hook-context` | `dunkingsheep memory hook-context \|\| deja hook-context` | the `\|\|` branch |
| opencode | `~/.config/opencode/AGENTS.md` deja rule | memory command/tools, deja read-only fallback | — |
| Codex | `~/.codex/AGENTS.md` deja rule | memory command/tools, deja read-only fallback | — |

The daemon side is unchanged: `memory_recall`, `memory_context`, `memory_search`,
`memory_stats`, `memory_status`, and the gated `memory_ingest` / `memory_backfill`
/ `memory_backfill` live on the existing `dunkingsheep` MCP server and CLI.

## Recall quality (bounded parity, `tools/memory_parity.py`)

26-file bounded backfill → 33 sessions / 58,191 chunks; 30 golden queries on
claude/codex/opencode, deja `--json` restricted to the same sessions.

| Metric | Dunking Sheep memory | deja |
| --- | --- | --- |
| recall@5 | **1.000** | 0.733 |
| exact top-5 set overlap | 0.717 | — |
| top-1 match | 0.667 | — |
| p50 / p95 | 6.6 ms / 13.9 ms | — |

Full corpus after backfill: 488/488 sources, 0 failed, 0 skipped, 326 sessions,
262,299 chunks, 277.9 MB. Live 20-client recall on the full corpus: 0 errors,
p50 57 ms, p95 109 ms, p99 144 ms. Acceptance
(`tools/memory_acceptance.py --duration 60`): p95 123 ms, 23,311 queries,
0 failures. Rollback was proven: consistent backup → delete → 0 chunks → restore
→ identical counts, with deja answering throughout.

## Rollback (any agent, in under a minute)

Deja never stopped and its index is intact, so rollback is a config edit:

1. **pi context**: in `~/.pi/agent/extensions/deja.ts`, `loadContext` already
   falls back to deja. To force deja-only, make `loadContext` call
   `runHookContext(pi, ctx, DEJA_BIN, ["hook-context"])` and return.
2. **pi tools**: in `~/.pi/agent/mcp.json`, drop `toolExposure` from the
   `dunkingsheep` entry (memory tools go back to deferred) and keep/raise the
   `deja` entry to `direct`; `/reload`.
3. **Claude Code**: in `~/.claude/settings.json`, change the SessionStart
   command back to `/home/hunter/.local/bin/deja hook-context`.
4. **opencode / Codex**: restore the deja paragraph in their `AGENTS.md`.
5. Verify deja independently: `deja "<query>"` and `deja-health`.

Data rollback for memory itself: `dunkingsheep memory status`, then stop the
daemon and replace `~/.config/dunkingsheep/memory.db` from the pre-migration
backup. deja is unaffected by anything in that file.

## Why deja stays live

Bounded parity passed and the full backfill is complete, but sign-off is the
reviewer's call. Until it lands, deja is the escape hatch: read-only queries
keep working and no migration step deletes or mutates its index.
