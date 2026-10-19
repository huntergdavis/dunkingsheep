# Memory v1 — scoped sign-off (ds4/pi, 2026-10-19)

## SIGNED: option (a) — scoped factual statement, NOT a parity claim

> **"Memory v1 is the primary recall path. Deja remains installed and read-only
> as a fallback. No parity claim is made: memory is NOT asserted to match deja's
> rankings, and the earlier '1.000 vs 0.733' headline is retracted."**

This is signed **as a factual/scope statement only**. The *product* choice of
which system is "primary" is not something the retrieval evidence can force; see
"what this signature does NOT mean" below.

## Evidence I checked independently (not the author's numbers on faith)

1. **Deja is answering again.** `deja --json "poolrad strict checkpoint"` →
   rc=0 in ~3.1 s with real session results. Root cause accepted and fixed:
   13 orphaned `deja mcp` servers, plus `deja-health --repair` rebuilding the
   whole index on any transient probe failure (the old AGENTS.md told agents to
   run it on recall failure, causing repeated full rebuilds — exactly the >15 min
   hang I hit). `--repair` now kills orphans (PPID 1, >1 h) and rebuilds only on a
   stale staging dir; the periodic unit is log-only.
2. **I re-ran the fixed harness myself** (`tools/memory_parity.py`, live-copy
   default, 2.19.2):
   - live-copy mode, **no re-ingest** (1.29 s), read-only against the live DB;
   - **60** golden queries, all scored: **memory recall@5 0.9667**,
     **deja recall@5 0.3167**, **mean top-5 overlap 0.31**, **top-1 0.2667**,
     **deja_errors 0**;
   - **intent (non-verbatim, n=12): mean top-5 overlap 0.0**, deja_errors 0;
   - **rollback passed**;
   - **overall gate FAILED** — correctly, because the parity gate now requires
     `mean_top5_overlap >= 0.50` in addition to memory self-recall, so the
     harness no longer blesses "memory alone did fine" as parity.
   These agree with the retraction (author: 1.000/0.367/0.336/0.30 on n=30;
   differences are sample size).
3. **Gate fixes verified in code**: `deja_errors` is counted and any deja error
   now **fails** the gate (the old silent skip is gone); per-query detail is
   emitted; the intent set exists.
4. **Full-corpus counts** (earlier, still standing): live DB = 488 sources /
   326 sessions / 262,299 chunks (265.0 MiB), and a SQLite `.backup()` → restore
   round-trip reproduces identical counts in ~2.4 s.

## Caveats recorded with the signature

- **No ground truth exists for "quality".** The golden set is verbatim slices of
  indexed chunks, so memory's 0.9667 is near-tautological — it proves the index
  works, not that it is a good retriever. The intent set has **no expected
  session**, so its `memory_recall5` / `deja_recall5` are **0.0 by construction**
  and only the *overlap* field is meaningful; it measures "do the two systems
  agree", not "is either right".
- **Deja is handicapped in this comparison.** Results are restricted to memory's
  ingested sessions (326) while deja indexes ~21,704; deja is scored only on the
  subset memory happened to ingest. So "memory > deja" holds **for this corpus and
  this restricted candidate set**, and is not a general claim about deja's value.
- **Deja's low score is on verbatim queries**, where it returns *different*
  sessions — a ranker disagreement, not necessarily an error.

## What this signature does NOT mean

- It does **not** say memory's retrieval is as good as or better than deja in
  general.
- It does **not** authorise deleting deja or its data. Keeping deja installed and
  read-only (as option (a) states) is exactly right: it preserves a reference
  ranker for future measurement and a fallback if memory regresses.
- It does **not** certify the `memory_recall5 >= 0.90` gate as a parity gate; that
  gate is a memory self-recall bar.

## What would strengthen this (follow-ups, not blockers)

1. Add a **ground-truth intent set** (queries with a known-correct session) so
   recall is measurable on non-verbatim intent; the current intent recalls are 0
   by construction.
2. Publish the **per-query detail alongside any future headline**, and never quote
   recall without the overlap next to it.
3. If "no parity claim" is the outcome, **remove or relabel** any lingering text
   that implies parity, so the retraction is unambiguous in the record.
4. Keep the new orphan-MCP cleanup + stale-staging rebuild rule; the observed
   failure mode (repeated full rebuilds triggered by a repair command in
   AGENTS.md) should not survive into whatever replaces deja.
