# Ops + Poolrad snapshot schema (for approval before the UI)

One file, `data/ops-snapshot.json`, written by a collector on a 10–15 min timer.
The page renders **only** this file: no model, no `gh`, no `git` at render time.
Every number carries `as_of`; a number that cannot be computed is `null` with a
`reason`, never a zero and never yesterday's value.

## Top level

```json
{
  "schema": "ops-snapshot/1",
  "generated_at": "2026-10-21T19:52:11Z",
  "oldest_as_of": "2026-10-21T18:05:00Z",
  "collector": {"duration_s": 3.4, "sources_ok": 6, "sources_failed": 1},
  "warnings": [
    {"source": "github", "reason": "gh not authenticated; PR counts are null"}
  ],
  "sources": {
    "github":   {"status": "ok",       "as_of": "2026-10-21T19:52:08Z", "detail": "1 pr list call"},
    "checkpoint": {"status": "ok",     "as_of": "2026-10-21T19:52:09Z", "detail": "function_accounting_checkpoint.json"},
    "tasks":    {"status": "ok",       "as_of": "2026-10-21T19:52:09Z"},
    "metrics":  {"status": "ok",       "as_of": "2026-10-21T19:52:09Z"},
    "panes":    {"status": "ok",       "as_of": "2026-10-21T19:52:10Z"},
    "hybrid":   {"status": "nulled",   "as_of": null, "reason": "hybrid_status.json missing"},
    "crackdown":{"status": "ok",       "as_of": "2026-10-21T19:52:10Z"},
    "freemodel":{"status": "nulled",   "as_of": null, "reason": "par-d tally not published yet"}
  },
  "agents": [ ... ],
  "types": [ ... ],
  "projects": [ ... ]
}
```

- `oldest_as_of` is the minimum `as_of` across every cell on the page. The page
  shows it next to the freshness clock, so a stale number is visible.
- `warnings` is derived from any null cell and any `sources[].status != "ok"`.
  The page renders it as a bar at the top; it is not a hidden tooltip.

## A cell

Every number is the same shape. That is what makes "cannot compute" show as `-`
with a reason instead of a plausible zero.

```json
{"value": 12, "as_of": "2026-10-21T19:52:08Z", "reason": null}
{"value": null, "as_of": null, "reason": "gh unauthenticated; no PR list"}
```

`value` is an integer (or a float for FPS), `as_of` is an ISO timestamp, and
`reason` is required when `value` is null. A cell with `value` set and `reason`
non-null is invalid.

## Agents (one row per agent)

```json
{
  "agent": "poolrad-par-a", "pane": "wW:p1", "type": "claude",
  "merged_prs":       {"value": 12, "as_of": "...", "reason": null},
  "bodies_credited":  {"value": 5,  "as_of": "...", "reason": null},
  "tasks_completed":  {"value": 3,  "as_of": "...", "reason": null},
  "messages_handled": {"value": 120,"as_of": "...", "reason": null},
  "free_model": {
    "accepted": {"value": null, "as_of": null, "reason": "par-d tally pending"},
    "rework":   {"value": null, "as_of": null, "reason": "par-d tally pending"},
    "rejected": {"value": null, "as_of": null, "reason": "par-d tally pending"},
    "failed":   {"value": null, "as_of": null, "reason": "par-d tally pending"}
  }
}
```

- `type` is one of `claude`, `codex`, `pi`, `opencode`, `free-model`.
- `free_model` is `null` for non-free-model agents, and the four cells otherwise
  (all null with a reason until par-d publishes its tally).
- `merged_prs` attribution: one `gh pr list` call for the window, then the PR
  list is attributed by **the same rule the board already uses** (title/label
  agent token, else author). If a PR cannot be attributed it is counted in
  `agents_unattributed` (below) and *not* guessed onto a pane.

## Types (one row per agent TYPE, aggregated from agents)

```json
{"type": "claude", "agents": 3,
 "merged_prs": {"value": 31, "as_of": "...", "reason": null},
 "bodies_credited": {"value": 12, "as_of": "...", "reason": null},
 "tasks_completed": {"value": 9, "as_of": "...", "reason": null},
 "messages_handled": {"value": 410, "as_of": "...", "reason": null}}
```

Aggregation is a sum of the per-agent cells, and a type cell is null (with the
reason "N agent cells are null") if any member cell is null — a type total that
silently omits an unreachable agent is a guess.

## Projects (one bar per big project)

```json
{
  "id": "poolrad", "label": "Poolrad",
  "metric": "strict bodies",
  "value": {"value": 280, "as_of": "2026-10-21T19:52:09Z", "reason": null},
  "denominator": {"value": 815, "as_of": "2026-10-21T19:52:09Z", "reason": null},
  "denominator_label": "CEILING",
  "evidence_ref": "poolrad-decomp/docs/re/function_accounting_checkpoint.json (reviewed_commit df9ca4e0)",
  "truth": "815 is an upper bound, not a verified denominator."
}
```

The four projects and their deciding numbers:

| id | metric | denominator | truth line |
| --- | --- | --- | --- |
| `poolrad` | strict bodies | 815, labelled **CEILING** | “815 is an upper bound, not a verified denominator.” |
| `hybrid` | credited bodies running as our C | bodies reached | “Running as our C is the claim; credited is the input.” |
| `reproducibility` | registered checkers | 280 | “A checker is registered when it fails a known-bad build.” |
| `crackdown` | measured FPS | 30 (target) | “Measured on <configuration>; a different config is a different number.” |

`crackdown` also carries `"configuration": "box64 <sha>, <flags>"` so the FPS
number is never read without the configuration that produced it.

## Collector rules (the schema assumes them)

1. **One `gh pr list` call** for the window; one read of the checkpoint; one
   query per database (`tasks.db`, `metrics.db`, `inbox.db`); one pane list.
   No per-agent `gh` call, no per-agent query.
2. **Idempotent**: running twice in the same window produces the same numbers
   (the PR window is explicit, not "since the last run").
3. **Fail loudly**: a source that raises/times out produces null cells with the
   reason and a `warnings` entry; the collector never substitutes a previous
   snapshot or a zero.
4. **Never a model**: attribution and aggregation are SQL/regex. No LLM at
   collect time either.

## Explicitly out of scope for v1

- Per-PR cost, token spend, and subscription cost (different sources, different
  cadence; the ops dashboard already has them and they stay there).
- Historical charts. v1 is one snapshot; a series can be added later without
  changing a cell.
- Any write to a project repo. The collector is read-only.
