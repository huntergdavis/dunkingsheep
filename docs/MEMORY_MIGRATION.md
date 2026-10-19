# Memory migration: agents off deja onto Dunking Sheep memory (t11)

Status (2026-10-19, VP direction): **memory primary, deja installed read-only,
no parity claim.** Dunking Sheep memory is the primary recall path for every
agent. Deja stays installed, answering capped, and **read-only**. We make **no
parity claim** (the earlier `1.000 vs 0.733` headline is retracted — see the
correction below). Deja is **not retired** until the reviewer signs a scoped
statement: **(a)** "memory primary, deja installed read-only, no parity claim"
or **(b)** "keep deja primary". Nothing was uninstalled, so every switch is
reversible by editing one file.

## Standing comparison

`python3 tools/memory_parity.py` is the standing comparison: it copies the live
DB read-only (full corpus, no re-ingest), queries deja live on the same
sessions, and reports rank agreement plus a hand-written **non-verbatim intent**
query set. Any deja error fails the gate. Deja queries are capped (`nice -n 10`,
`GOMAXPROCS=2` via `~/.local/bin/deja`) and each harness query has a timeout.
Report: [`MEMORY_PARITY_REPORT.md`](MEMORY_PARITY_REPORT.md); per-query detail:
[`memory_parity_detail.json`](memory_parity_detail.json).

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

**Correction (reviewer feedback):** the earlier headline `recall@5 1.000 vs
deja 0.733` was bounded-sample and near-tautological (golden queries were
verbatim slices of indexed chunks). On the full corpus with deja live, the
honest numbers are: memory recall@5 1.000 (sanity check only), deja recall@5
0.367, mean top-5 overlap 0.336, top-1 0.30; on hand-written non-verbatim
intent queries deja returns nothing for most queries while memory stays
on-topic. **No parity claim.** Full methodology and per-query detail:
[`docs/MEMORY_PARITY_REPORT.md`](MEMORY_PARITY_REPORT.md) and
[`docs/memory_parity_detail.json`](memory_parity_detail.json).

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

The full backfill is complete and every agent is on memory, but retirement is
the reviewer's call and no parity is claimed. Until they sign, deja is the
escape hatch: read-only queries keep working and no migration step deletes or
mutates its index. A hang here had two causes, both fixed: orphaned `deja mcp`
servers are now cleared by `deja-health --repair`, and `--repair` rebuilds only
when the staging dir is stale (it used to rebuild on any problem, so agents
following the old "run --repair" advice were kicking off full reindexes).
