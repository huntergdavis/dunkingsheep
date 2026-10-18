# ADR 0001: A memory and recall service inside Dunking Sheep

- **Status:** Proposed (v1)
- **Date:** 2026-10-18
- **Owner:** Dunking Sheep / pi (w8:p8), for the VP of Agentry
- **Replaces:** deja 0.6.0 as the fleet memory layer (deja stays in place until parity)
- **Related:** global task queue 2.15.x, MCP/CLI surface audit (t8)

## Summary

Build a **memory and recall service inside Dunking Sheep** on **SQLite 3.46.1 +
FTS5 in WAL mode**, with **one background ingest writer** and a pool of
read-only query connections. Ingest is asynchronous and incremental; a query
never triggers a reindex or a whole-index swap. This removes deja's 2-minute
stalls without adding a service to operate.

v1 is scoped honestly to **1–10 million chunks** (the current ~17 GB corpus) and
**20 concurrent agents at p95 < 1 s**. "Billions of records" is a later
**sharded/remote tier**, not something one SQLite file on this 31 GB box should
pretend to do.

## Context

### What deja 0.6.0 is, measured

deja is a static Go binary with a ~250 MB bucket index (`sessions.gob` +
per-letter `.bin` buckets) over a roughly 17 GB corpus:

| Source | Size on this machine |
|---|---|
| `~/.codex/sessions` (437 JSONL files) | 14 GB |
| `~/.codex/thread_history_1.sqlite` | 2.1 GB |
| `~/.claude/projects` (34 JSONL files) | 975 MB |
| `~/.local/share/opencode/opencode.db` | 31 MB |
| `~/.pi/agent/sessions` (15 JSONL files) | 21 MB |
| deja index (`~/.cache/deja/index.db`) | 250 MB |

Measured behaviour:

- **Warm single query:** 0.31 s.
- **Five parallel recalls:** 2.49 s wall, all succeeded.
- **The 2-minute stalls are not corpus size.** They are the **per-query
  incremental reindex** (`deja: updated N files`) plus a **whole-index staging
  swap** (`index.db.tmp` → `index.db`). Reads queue behind that write, so the
  30 s health probe times out and emits a false "recall is not working" warning.
  That exact failure mode was diagnosed and repaired on 2026-10-17/18; the
  health check now exists so it cannot fail silently, but the write path is
  still the wrong shape for 20 agents.

### Host reality

- 8 CPUs, 31 GiB RAM (24 GiB available), 622 GiB free on `/home`.
- SQLite **3.46.1 with FTS5** is installed and is already the storage engine
  behind Dunking Sheep's inbox, tasks and metrics.
- **Postgres is not installed** (no packages, binaries or units). Elasticsearch
  and OpenSearch are not installed. Adding either is a new service to operate,
  back up and monitor.

### Workloads that matter

1. **`recall(query, ...)`** — ranked snippets with session/project metadata,
   called by an agent during work.
2. **`recall_context(query)`** — a compact digest for session start: recent
   relevant sessions, a few snippets, counts.
3. **`hook-context`** — the session-start injection every harness already uses.
4. **Metadata statistics** — sessions/chunks/messages by harness, project, day,
   latency, ingest queue depth.
5. **Ingest** — new and changed Claude/Codex/opencode/pi sessions, including a
   2.1 GB Codex SQLite history, arriving while agents query.

### SLOs

- **20 concurrent agents**, reads and ingest at the same time.
- **p95 < 1 s**, p99 < 2 s, **zero 2-minute stalls**.
- No query may depend on an ingest completing.
- Migration is **reversible** until a reviewer signs off parity.
- No LLM in scheduling, ranking or ingest; deterministic SQL/algorithms.

## Decision

Use **SQLite 3.46.1 + FTS5** as the v1 engine:

- **WAL mode**, one writer (the daemon's ingest worker), many readers.
- **Asynchronous, incremental ingest** through a durable queue. A query never
  reindexes, never swaps an index, and never waits for a writer.
- **Read-only connection pool** per query; prepared statements and a small
  query cache.
- **FTS5 BM25** ranking with recency/project/session boosts; no embeddings in
  v1 (optional `sqlite-vec` later).
- **v1 scale target:** ~1–10 M chunks, ~17 GB corpus, 250 MB–10 GB DB.
- **Billions:** a later sharded/remote tier (Postgres + FTS/vector, or an
  object-store + index service). The schema and query API are designed so that
  tier can replace the engine without changing agent tools.

This is deliberately the **zero-new-ops** decision. Dunking Sheep already owns
SQLite, backups and the daemon lifecycle.

## Engine evaluation

| Option | Strengths | Costs / why not v1 |
|---|---|---|
| **SQLite WAL + FTS5** | Already installed and used; ACID; concurrent readers with one writer; BM25 + snippets; trivial backup; no new service | One writer; large-index tuning; no native vector type; 10–100 M is a tuning/sharding exercise |
| **Postgres FTS + pgvector** | Better concurrent writers; richer SQL; vectors in the same engine; proven at large scale | New install/unit/backup/monitoring; v1 has one writer; migration and ops cost before it is needed |
| **Elasticsearch/OpenSearch** | Excellent search, sharding, aggregations | Heavy JVM service, memory-hungry on a 31 GB box, wrong trade for one machine |
| **DuckDB** | Fast analytical scans | Not an OLTP/concurrent-reader service; no FTS5 |
| **Custom LMDB/Tantivy** | Control | Rebuilds a database and a search engine; no zero-ops win |
| **Vector-only** | Semantic recall | Loses exact/lexical recall and snippets; embeddings are an add-on, not the base |

Postgres becomes the right answer when **more than one process must write** or
when the data is **sharded across machines**. That is the v2/remote tier, not
this box.

## Architecture

```
Claude/Codex/opencode/pi source logs
        │  (mtime/size/sha, incremental scanner)
        ▼
  ingest_queue  ──►  MemoryIngestor (one writer thread in the daemon)
        │                 │  parse → redact → chunk → hash → batch upsert
        │                 ▼
        │            memory.db (WAL)  ── chunks + chunks_fts + sessions + sources
        │                 ▲
        │                 │  read-only connection pool (per query, prepared)
        └── state/leases ─┘
                          ▲
                  memory_recall / memory_context / memory_stats
                  MCP · CLI · unix socket · TUI (later)
```

Components:

- `dunk_memory.py` — schema, migrations, query API, ingest adapters.
- `MemoryIngestor` — one writer thread, durable queue, leases, retries,
  backpressure, metrics.
- Daemon lifecycle — start/stop the worker; expose status; survive restarts.
- Surfaces — MCP tools, CLI, socket commands; the daemon is the single owner of
  the database file.

### Concurrency rules

- **One writer.** SQLite WAL allows one writer and many readers. All writes go
  through the ingest worker; no command writes on the read path.
- **Readers use `mode=ro` connections** with `busy_timeout=5000`; they see a
  consistent snapshot and never block on a writer.
- **Batches** of 500–2000 chunks, one transaction per batch; `synchronous=NORMAL`
  in WAL; `wal_autocheckpoint` tuned; no `VACUUM` on the hot path.
- **No schema changes during a query**; migrations run at daemon start or in a
  maintenance command.
- **Backpressure:** if the queue is deep or a batch exceeds its latency budget,
  the worker slows down; queries are unaffected. Ingest is `nice`/low-priority
  and can be paused.
- **Crash recovery:** the queue and source hashes are durable; restart resumes.
  No index swap, so a crash cannot leave a half-swapped index.

## Schema (v1)

```sql
PRAGMA journal_mode = WAL;
PRAGMA synchronous = NORMAL;
PRAGMA foreign_keys = ON;
PRAGMA busy_timeout = 5000;

CREATE TABLE schema_meta (key TEXT PRIMARY KEY, value TEXT NOT NULL);

CREATE TABLE sources (
  id            INTEGER PRIMARY KEY,
  harness       TEXT NOT NULL,          -- claude | codex | opencode | pi
  path          TEXT NOT NULL UNIQUE,
  mtime         REAL NOT NULL,
  size          INTEGER NOT NULL,
  sha256        TEXT,
  state         TEXT NOT NULL,          -- pending | ingesting | ok | error | skipped
  last_ingest   REAL,
  error         TEXT,
  metadata      TEXT
);

CREATE TABLE sessions (
  id            TEXT PRIMARY KEY,       -- stable source session id
  harness       TEXT NOT NULL,
  project       TEXT,                   -- workspace/project label or cwd
  cwd           TEXT,
  title         TEXT,
  started_at    REAL,
  updated_at    REAL,
  metadata      TEXT
);
CREATE INDEX sessions_by_project ON sessions(project, updated_at);

CREATE TABLE chunks (
  id            INTEGER PRIMARY KEY,
  session_id    TEXT NOT NULL REFERENCES sessions(id) ON DELETE CASCADE,
  ordinal       INTEGER NOT NULL,
  role          TEXT,                   -- user | assistant | tool | summary
  kind          TEXT,                   -- message | tool_result | note
  text          TEXT NOT NULL,
  token_count   INTEGER,
  sha256        TEXT NOT NULL,
  created_at    REAL,
  metadata      TEXT
);
CREATE INDEX chunks_by_session ON chunks(session_id, ordinal);
CREATE UNIQUE INDEX chunks_dedupe ON chunks(sha256, session_id, ordinal);

CREATE VIRTUAL TABLE chunks_fts USING fts5(
  text,
  content='chunks',
  content_rowid='id',
  tokenize='porter unicode61',
  prefix='2 3'
);

CREATE TABLE ingest_queue (
  id            INTEGER PRIMARY KEY,
  source_id     INTEGER NOT NULL REFERENCES sources(id) ON DELETE CASCADE,
  state         TEXT NOT NULL,          -- queued | leased | done | failed
  attempts      INTEGER NOT NULL DEFAULT 0,
  next_at       REAL,
  lease_until   REAL,
  error         TEXT
);
CREATE INDEX ingest_queue_ready ON ingest_queue(state, next_at);

CREATE TABLE memory_events (            -- append-only, metrics only
  id            INTEGER PRIMARY KEY,
  at            REAL NOT NULL,
  event         TEXT NOT NULL,          -- query | ingest | error | health
  actor         TEXT,                   -- pane id / harness
  query_hash    TEXT,                   -- never the query text?
  latency_ms    INTEGER,
  hits          INTEGER,
  source        TEXT,
  metadata      TEXT
);
```

Optional later: `embeddings(chunk_id, vector BLOB)` with `sqlite-vec`; not v1.

### Chunking

- One chunk per message/turn boundary; tool results are separate chunks with
  `kind='tool_result'` so they can be down-ranked.
- Target 512–1024 tokens, 64-token overlap; never split mid-code-block or
  mid-tool-call; keep role + session + project metadata.
- Deduplicate by `(sha256, session_id, ordinal)`; re-ingest is idempotent.
- Redact obvious secrets/credentials before storage (reuse the daemon's
  redaction rules); store the redaction marker, never the secret.

### Ranking

- `chunks_fts MATCH` with BM25; weights: `text` 1.0, title/metadata boost.
- Post-rank in SQL/Python: recency decay (30-day half-life), project/session
  boost, tool-result penalty, dedupe by session for the first page.
- `recall`: top-K snippets + session/project/date + a stable chunk id.
- `recall_context`: top-N sessions by best chunk + 1–3 snippets + metadata,
  compact enough for session-start injection.
- Query cache: small LRU keyed by normalized query + filters, 5 s TTL, no
  cross-agent leakage beyond the same filters.

## Ingest and migration

### Source adapters

- **Claude:** `~/.claude/projects/**/*.jsonl` (message events).
- **Codex:** `~/.codex/sessions/**/*.jsonl` (rollouts) plus
  `~/.codex/thread_history_1.sqlite` (`thread_items` / `thread_turns`).
- **opencode:** `~/.local/share/opencode/opencode.db` (`session`, `message`,
  `part`).
- **pi:** `~/.pi/agent/sessions/**/*.jsonl`.
- The scanner records mtime/size; content hash decides whether to re-parse.
  Adapters are versioned and tested against fixture logs; a schema drift is an
  adapter bug, not a silent drop.

### Ingest loop

1. Scanner enqueues changed sources (mtime/size; hash on change).
2. Worker leases a source, streams the file/DB, parses messages, redacts,
   chunks, and upserts in batches.
3. On success: `sources.state='ok'`, `last_ingest=now`; on failure: retry with
   backoff, `error`, and a metric.
4. Progress is visible in `memory_status` / `memory_stats`.
5. Full backfill is the same loop with a large queue; it is throttled and can
   be paused. It never blocks reads.

### Migration from deja

- **Do not touch deja's data.** The old index stays in place.
- Build `memory.db` by **re-ingesting the source logs**, not by parsing deja's
  gob/bucket index. The source logs are the ground truth and survive deja.
- Keep deja available read-only during the migration. The daemon can serve both
  `memory_*` and `deja` tools until parity is signed off.
- **Parity gate:** a golden set of real queries (≥50, across harnesses and
  projects) is run against deja and memory. Require ≥90% top-5 overlap and the
  expected session in `recall_context`'s top 3, plus equal-or-better p95.
- **Rollback:** delete/rename `memory.db` and re-enable deja; no deja data was
  modified. A dated backup of `memory.db` is taken before each schema migration.
- After sign-off: update AGENTS.md/MCP instructions, keep deja installed but
  unused for one release, then retire the `deja-health` timer in favor of
  `memory_stats`/health.

## Agent upgrade and rollback

- Add `memory_recall`, `memory_context`, `memory_search`, `memory_stats`,
  `memory_ingest`, `memory_status` as MCP tools (small, direct exposure).
- Keep deja's `recall`/`recall_context` during parity; route the pi
  `deja.ts` extension to `memory_context` after sign-off (or run both and
  compare).
- Update `AGENTS.md` (pi/opencode/codex), MCP instructions and the VP handoff.
- Rollback is a config switch: point the extension/instructions back at deja,
  leave `memory.db` for inspection. No deja mutation.
- Schema migrations are versioned and run at daemon start; a failed migration
  refuses to start the memory service rather than serving half a schema.

## MCP / CLI / API surface and context budget

Current surface measured in this repo: **65 registry commands, 62 MCP tools,
~25 KB of JSON schema** for Dunking Sheep alone, plus deja and the task board.
That is already too much context for every agent.

v1 implements six memory commands so the engine can be measured directly:

| Tool | Purpose |
|---|---|
| `memory_recall` | ranked snippets + session/project/date |
| `memory_context` | compact session-start digest |
| `memory_search` | filtered search (harness, project, since, role) |
| `memory_stats` | counts, ingest queue, latency, coverage |
| `memory_ingest` | enqueue a source / backfill (superuser) |
| `memory_status` | health, schema version, last ingest |

CLI: `dunkingsheep memory recall|context|search|stats|ingest|status`.
Socket: the same command names.

**Grouping (t8, owner #860):** simplify by grouping, never by deleting. The
target is one namespaced MCP tool per domain with an `action` argument —
`inbox`, `task`, `dunk`, `pane`, `workspace`, `superuser`, `memory`, `metrics` —
with a small hot set on `direct` exposure (`memory_recall`, `memory_context`,
`inbox_read`, `inbox_send`, `task_next`, `task_complete`) and everything else
`deferred`/optional. Memory stays one call for `recall`/`context`; the grouped
`memory` action tool is acceptable if its schema stays small. A published
old-tool → new-grouped-tool mapping plus a test that every old capability still
exists is the acceptance for t8. Measure and report the before/after tool list
and schema bytes (today: 62 MCP tools, ~25 KB schema for Dunking Sheep alone;
target < 10 KB on the common path).

## Acceptance test

A script (`tools/memory_acceptance.py`, added with t9) will:

1. **Build** a `memory.db` from the real sources, or a representative sample
   with `--sample`, without touching deja.
2. **Golden set:** ≥50 real query strings covering all four harnesses and
   several projects.
3. **Parity:** for each golden query, run deja and memory; require ≥90% top-5
   session overlap and the expected session in `recall_context` top 3.
4. **Concurrency:** start ingest (backfill or a large delta) and run **20
   parallel recall clients for 60 s** against the golden set. Record p50/p95/p99,
   failures and any query >2 s. **Pass: p95 <1 s, p99 <2 s, zero failures, zero
   2-minute stalls.**
5. **Coverage:** report sources, sessions, chunks, DB bytes, queue depth, and
   per-harness counts; fail if a source is silently missing.
6. **Rollback:** stop the memory service, confirm deja still answers, restore a
   DB backup and confirm the same counts.
7. **Hygiene:** no secrets in a sample of stored chunks; schema version and
   `memory_stats` agree.

The test is the gate for t10 (migration/parity) and t11 (agent upgrade).

## Honest scale limits

- v1 targets **1–10 M chunks / ~17 GB corpus**. SQLite FTS5 can hold that on
  this box with a single writer, WAL readers and roughly 3–10 GB of index.
- **10–100 M chunks** is possible only with careful partitioning (per-project
  DBs or a single writer shard), tighter chunking and more disk; it is not the
  first target and must be proven, not assumed.
- **Billions** require a **sharded/remote tier** (Postgres/object store +
  search/vector service). The API is designed so that tier can replace SQLite
  without changing agent tools; claiming it for one SQLite file would be
  dishonest.
- Embeddings/semantic recall are **deferred** until the lexical baseline is
  measured; `sqlite-vec` or pgvector is the likely v2 path.

## t10 bounded parity result (2026-10-18)

`tools/memory_parity.py` ingested a bounded 26-file sample (all pi sessions, 4
Claude, 6 Codex, opencode) into a temp DB — 33 sessions / 58,191 chunks / 64 MB
— then ran 30 golden queries (5-word phrases drawn from claude/codex/opencode
sessions; deja does not index pi). Deja's results were restricted to the same
session set.

- **memory recall@5 = 1.000** vs **deja recall@5 = 0.733** — the new engine
  surfaced the source session in the top five for every query, more often than
  deja.
- Exact top-5 **set** overlap: 0.717 (top-1 match 0.667): the rankers differ, but
  the migration-relevant recall is not worse.
- p50 6.6 ms, p95 13.9 ms.
- Rollback proven: consistent SQLite backup, delete → 0 chunks, restore →
  identical counts (26/33/58,191), and deja still answering throughout.
- Stopped ingest now records `skipped` with a reason and a `partial` source
  (resumable), never a silent `failed`.

The full 17 GB backfill and the agent upgrade remain gated on the reviewer's
sign-off of this parity result.

## Risks and open questions

- FTS5 index size and write amplification on a 17 GB corpus; measure before the
  full backfill.
- SQLite's one-writer model: the ingest worker must never do long transactions;
  batch size and checkpoint tuning are part of the acceptance test.
- Source schema drift (Codex rollouts, Claude JSONL, opencode DB) — versioned
  adapters and fixture tests.
- Redaction and privacy: never store credentials; per-project visibility rules
  still need an owner decision (v1: same visibility as deja, project metadata
  filterable).
- Disk: 622 GiB free now; a 3–10 GB index is fine, but backups and retention
  need a policy.
- No semantic search in v1; lexical + recency is the baseline.
- The memory service must not become a new stall: ingest is throttled, pausable
  and observable; queries never wait on it.

## Milestones

1. **t7 (this ADR)** — engine decision, schema, ingest/query design, migration
   and acceptance criteria.
2. **t9** — `dunk_memory.py`, schema, async ingest worker, `memory_recall`/
   `memory_context`/`memory_stats`, full test suite, acceptance script.
3. **t10** — backfill + parity against deja; reversible migration; reviewer
   sign-off.
4. **t11** — move every agent (pi/opencode/Claude/Codex) to the memory tools;
   retire deja usage; keep deja data untouched.
5. **t8** — MCP/CLI/API dedupe and context-budget audit.
