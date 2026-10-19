# Memory migration: deja retired (t11, owner #913)

Status (2026-10-19, owner #913): **deja is fully retired.** Dunking Sheep memory
is the **only** recall path for every agent. There is no deja fallback, MCP
entry, startup hook, timer, or binary on `PATH`. Deja's index and data are
**archived, not deleted** (do not remove them).

The earlier "memory primary, deja read-only, no parity claim" scoped statement
is historical; owner #913 superseded it by removing deja from the stack.

## What each agent now uses

| Agent | Recall path now |
| --- | --- |
| pi startup context | `dunkingsheep memory hook-context`, injected as the hidden `memory-context` message (`~/.pi/agent/extensions/memory.ts`) |
| pi tools | `dunkingsheep` MCP: `inbox`/`task`/`memory`/`metrics` direct; memory actions `recall`/`context`/`search`/`stats`/`status` |
| Claude Code | SessionStart → `dunkingsheep memory hook-context` (no `\|\|` fallback) |
| opencode | `AGENTS.md` → `dunkingsheep memory ...` / memory MCP tools |
| Codex | `AGENTS.md` → `dunkingsheep memory ...` / memory MCP tools |

No agent config lists deja any more: pi `mcp.json`, `~/.claude/settings.json`,
`~/.codex/config.toml`, `~/.config/opencode/opencode.jsonc`, and the AGENTS
files are clean.

## Retirement audit

`python3 tools/check_deja_free.py` (and `--repo-only` in CI) fails if an active
surface still references deja: harness configs/hooks/instructions, systemd user
units, PATH binaries, scheduled dunks, or Dunking Sheep runtime code. Historical
docs/tests and the archival comparison are reported, not failed.

Result on 2026-10-19: **ok** — no active references; historical references only
in docs/tests and the archival comparison harness.

## What was removed or archived

- **Configs/hooks:** deja MCP entries removed from pi, Claude, Codex and
  opencode; the Claude SessionStart `\|\| deja` fallback removed; pi extension
  renamed `deja.ts` → `memory.ts` and made memory-only.
- **Timer:** `deja-health.service`/`.timer` and its drop-in moved to
  `~/.local/share/deja-retired/`; the user timer list has no deja unit.
- **Binaries:** `deja`, `deja-real` and `deja-health` are no longer on `PATH`
  (archived by the VP; the wrapper is not installed).
- **Instructions:** `AGENTS.md` (pi/codex/opencode) says the legacy index is
  retired and memory is the only recall path.
- **Data:** `~/.cache/deja/index.db`, the health report and the archived units
  are kept. Nothing deleted.

## Memory covers every recall use

The memory service indexes 488 sources / 326 sessions / 262,299 chunks. The
hand-labelled ground-truth intent set (24 queries; `INTENT_GROUND_TRUTH`) gives
**recall@5 = 0.9167 (22/24)** with p50 ~1.5 s / p95 ~3.5 s. The two misses are
the Claude weekly-allowance/C-porting query and the host-hold/high-PSI query
(see [`MEMORY_INTENT_SET.md`](MEMORY_INTENT_SET.md)). Those are the only known
coverage gaps; no query class depends on deja.

## Rollback

Deja rollback is **no longer supported**: the binaries are archived and no
config points at them. The real rollback is the memory DB itself — a consistent
SQLite backup can be restored over `~/.config/dunkingsheep/memory.db` (see the
rollback proof in the comparison harness). If deja ever had to be restored, the
archived units/binaries are in `~/.local/share/deja-retired/`, but that is an
archaeology step, not a supported path.

## Historical appendix

The bounded parity result (`recall@5 1.000 vs deja 0.733`) was **retracted**:
it was a bounded sample and the golden queries were verbatim slices. The
reviewer verified rollback and signed a scoped no-parity-claim statement
([`memory_signoff.md`](memory_signoff.md)); owner #913 then retired deja
entirely. Full comparison history:
[`MEMORY_PARITY_REPORT.md`](MEMORY_PARITY_REPORT.md) and
[`memory_parity_detail.json`](memory_parity_detail.json).
