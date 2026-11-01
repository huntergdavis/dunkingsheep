# Dunking Sheep in Rust: the implementation guide

**Status:** design, 2026-10-29. Owner's ask: port the whole daemon to Rust with
the exact same functionality, written so that a lower-tier agent can do the
conversion by following instructions serially, in a parallel worktree, with a
live cutover at the end. Companion files: `MANIFEST.md` (every module, class,
function, command, schema and test, generated from the tree) and `TASKS.md`
(the ordered task list, ready to be filed into the queue).

The Python tree this ports is `main` at the commit named in `MANIFEST.md`
(35k lines of code, 30k lines of tests, 132 test files). The port is
**behaviour-for-behaviour**: same wire protocol, same SQLite schemas, same
files on disk, same CLI surface and exit codes, same systemd units. Nothing an
agent, a hook, a timer or the web desk talks to changes. That is what makes a
live cutover possible: the Rust daemon is a drop-in replacement for one
process, verified by running both against the same recorded traffic.

---

## 0. Rules for whoever does a task (read before every task)

1. **One task, one file, one PR.** Each task in `TASKS.md` names the Rust
   file(s) it creates and the Python file(s) it ports. Do not touch other
   files except `Cargo.toml` dependencies named by the task.
2. **Port, do not redesign.** Keep names: `dunk_tasks.py::Tasks.claim_next`
   becomes `tasks::Tasks::claim_next`. Keep error messages byte-identical
   where a test asserts on them (the Python test tells you). Keep comments
   that explain a measured incident (the `t423`, `2026-10-24` style notes):
   they are the reasons the code is shaped that way.
3. **Tests first, from the Python tests.** Every task lists the Python test
   file(s) to port. Port each test function as a Rust `#[test]` (or
   `#[tokio::test]`) in the same order with the same name. A task is done when
   those tests pass, `cargo clippy -- -D warnings` is clean, and `cargo fmt`
   is clean. Never delete a test to make it pass; say in the PR which test you
   could not port and why.
4. **The differential harness is the final judge** (section 7). When your task
   touches a command, run `tools/diff_harness.py --commands <names>`; it
   replays recorded requests against the Python daemon and your Rust daemon
   and diffs the JSON. A diff is a bug in the port unless the task says the
   output legitimately differs.
5. **No `unsafe`. No new crates** beyond the list in section 3; if a task
   seems to need one, stop and say so in the PR.
6. **Errors:** every fallible function returns `Result<T, DunkError>`.
   `DunkError` has the variants in section 4.3; the text the client sees is
   `Display`. Never `unwrap()` on anything that comes from a file, a socket,
   a subprocess or a database; `expect("...")` only on invariants the task
   names.
7. **Logging:** `tracing` with the same message texts the Python logs use
   (the watchdog and tests grep them).
8. **Record what you did NOT verify** in the PR body (the fleet's standing
   rule: a claim one step wider than its evidence is the defect we see most).

---

## 1. Why Rust, concretely (what we buy, measured on the Python)

| Python cost today | Rust design | Effect |
| --- | --- | --- |
| Every request that needs a pane record shells out `tmux list-panes -a` (one subprocess, ~10-40 ms idle, 3-9 s per read at load 30 measured 2026-10-29 t535) | One **tmux control-mode client** (`tmux -C`) per server, kept open; pane/window/session state is held in memory and updated from control-mode notifications; `capture-pane` only on demand | Reads go from a subprocess per call to a hash-map lookup; the 3-9 s stalls under load disappear |
| One OS thread per socket connection, GIL-serialised; three background threads plus an outbox thread per pane | **tokio** multi-threaded runtime: async socket server, one task per connection, the loops are tasks on timers | 30 lanes hammering the socket no longer queue behind each other; the `ENFILE` / fd-leak class of bug (t142) goes away with owned connections |
| `sqlite3` connection per thread, created lazily, `BEGIN IMMEDIATE` for writers | **one writer actor per database** (`tasks`, `inbox`, `metrics`, `agent-state`, `owner-push`, `memory`), readers on a small pool; same WAL + `BEGIN IMMEDIATE` semantics | Lock contention becomes a queue, never a `database is locked` surprise; the 5 s busy timeout is a last resort, not the norm |
| JSON files rewritten with `os.replace` in 12 modules (15 sites), each with its own temp-file + chmod dance | one `atomic_json::write` | 15 copies become 1 |
| `time.time()` (145 sites), `except Exception` (167), ad-hoc retry loops (312 mentions) | `Instant`/`SystemTime` helpers, typed errors, one `retry::with_backoff` | smaller, and the behaviour is the same everywhere |
| Startup: interpreter + 35k lines imported on every CLI call (~1 s CPU per invocation measured in t535) | a static binary; the CLI is the same binary as the daemon (`dunkingsheep serve`) | `dunkingsheep inbox read` costs milliseconds; 30 lanes polling no longer cost a core |
| The MCP server, the CLI parser (362 `add_argument` lines) and the command registry (117 `Command(...)`) are three hand-maintained copies of the same surface | **one registry** (section 4.2); the CLI, MCP schema and socket dispatch are derived from it | 156 subparsers and 362 arguments are generated, not written |

What Rust does **not** change: the invariants. The role allowlist, the kernel
binding of a pane to its process, the task leases, the one-live-review-per-head
index and the ledger are the same rules with the same tests.

---

## 2. Workspace layout

```
rust/                              Cargo workspace at the repo root (Python stays beside it until cutover)
  Cargo.toml                       [workspace] members below; one lockfile; edition 2021; MSRV = stable
  crates/
    ds-core/                       library: everything the daemon and CLI share
      src/lib.rs
      src/error.rs                 DunkError (4.3)
      src/config.rs                CONFIG_DIR, paths, env switches (one place for every DUNKINGSHEEP_* variable)
      src/atomic_json.rs           read/write JSON files atomically with mode 0600 (replaces 15 sites)
      src/lock.rs                  fcntl file locks (dunk_superusers._locked), shared/exclusive
      src/retry.rs                 with_backoff(attempts, base) for socket connects and sqlite busy
      src/proc.rs                  /proc readers: stat, start time, ancestry walk (dunk_callers)
      src/registry.rs              the command registry: name, params, required, mcp, groups (4.2)
      src/protocol.rs              wire types: Request, Response (newline JSON)
      src/client.rs                FlockClient: connect, identity resolution, service token (dunk_client)
      src/roles.rs                 dunk_roles
      src/tasks/{mod,store,claim,next_job,metrics}.rs     dunk_tasks + dunk_next_job
      src/inbox/{mod,store,deliver}.rs                   dunk_inbox + delivery pieces of dunk_core
      src/terminal/{mod,tmux,control,parse}.rs           dunk_terminal (control-mode client, 5.1)
      src/panes.rs                 dunk_panes (lane registry, retired panes)
      src/activity.rs              dunk_activity (turn state)
      src/stuck.rs                 dunk_stuck
      src/dispatch.rs              dunk_dispatch
      src/owner.rs                 dunk_owner
      src/metrics.rs               dunk_metrics
      src/state.rs                 dunk_state
      src/load.rs, policy.rs       dunk_load, dunk_policy
      src/context/{mod,store,ledger,digest}.rs           dunk_context
      src/memory/{mod,ingest,recall}.rs                  dunk_memory (FTS5)
      src/github.rs, review.rs, poolrad.rs               dunk_github, dunk_review, dunk_poolrad
      src/watchdog/{mod,rules,deadman}.rs                dunk_watchdog, dunk_watchdog_deadman
      src/health.rs, usage.rs, views.rs, pm.rs, help.rs, allocator.rs, fallback.rs, recovery.rs
      src/flock/{mod,dunks,sends,commands_*.rs}          dunk_core::Flock, split by concern (5.3)
      src/api.rs                   dunk_api: handlers table, dispatch(), allowlist check, desk caps
      src/server.rs                dunk_server: socket server, identity binding, request loop
    ds-cli/                        binary `dunkingsheep`: clap parser GENERATED from the registry + the local-only commands
    ds-mcp/                        binary `dunkingsheep-mcp` (also reachable as `dunkingsheep mcp`): stdio JSON-RPC
    ds-tui/                        (phase 3) ratatui port of dunking_sheep_tui.py
    ds-web/                        (phase 3) axum port of web/app.py; the React ui/ is unchanged
  tools/diff_harness.py            records socket traffic from the Python daemon, replays to both, diffs (7)
```

Phase 1 (daemon + CLI + MCP, the whole socket surface) is the cutover unit.
Phase 2 ports the timers' subcommands that run out of process (watchdog,
dispatch tick, review sync, poolrad sync, state capture, health). Phase 3 is
the TUI and the web desk. The Python tree is deleted only after phase 3.

---

## 3. Crates (pinned; nothing else without a PR saying why)

| crate | use | why this one |
| --- | --- | --- |
| `tokio` (rt-multi-thread, net, sync, time, process, signal, io-util) | runtime, unix socket, timers, subprocess | the standard; one runtime for everything |
| `serde`, `serde_json` | wire protocol, JSON files | byte-compatible output with `json.dumps` when `serde_json::to_string` is used with `BTreeMap`-ordered keys where the Python used sorted keys (`sort_keys=True` sites are named in MANIFEST) |
| `indexmap` (via serde_json `preserve_order`) | object key order | owner-approved 2026-10-31: `serde_json::Map` keeps insertion order like a Python dict; code that must match `sort_keys=True` sorts explicitly |
| `rusqlite` (bundled, `fts5`) | all six databases | same SQLite, same WAL; bundled so the host's library version never matters; FTS5 for memory |
| `clap` (derive) | CLI | subcommands generated from the registry at build time (`build.rs`) |
| `tracing`, `tracing-subscriber` | logs | same texts as today |
| `thiserror` | DunkError | |
| `nix` (fcntl, socket SO_PEERCRED, signal) | file locks, peer pid, signals | the Python uses `fcntl.flock` and `SO_PEERCRED` |
| `notify` | watch `roles.json`, `service-tokens.json`, `dunks.json` | replaces stat-on-every-call refresh |
| `jsonwebtoken` (rsa) | GitHub App JWT (RS256) | replaces PyJWT |
| `reqwest` (rustls, json, blocking off) | GitHub REST | async, no OpenSSL dependency |
| `axum` (=0.8.9; `http1`, `json`, `tokio`) | ds-web HTTP routes | one Tokio-based Axum/Hyper HTTP stack in ds-web; ds-core remains transport-neutral |
| `ratatui` (=0.30.2; `std`, `crossterm_0_29`) | ds-tui rendering | maintained terminal UI widgets and layout; only the Crossterm backend is enabled |
| `crossterm` (=0.29.0; `events`, `windows`) | ds-tui input and terminal control | terminal events and control, using the same version selected by Ratatui's backend |
| `sha2`, `hmac` | ledger hashes, token hashes | |
| `rand` | tokens | `secrets.token_urlsafe` equivalent: 32 bytes base64url |
| `chrono` | ISO timestamps where the Python formats them | only at the edges; internally `SystemTime` |
| `regex` | the handful of compiled patterns (pane ids, box detection) | |
| `libc` | `getpid`, `umask` where `nix` lacks it | |
| dev: `tempfile`, `assert_cmd`, `insta` (snapshot tests for CLI output) | tests | |

Not used: an ORM, an async-sqlite wrapper (the writer actor is simpler and
matches the Python's lock discipline), a tmux crate (control mode is a
line protocol; we parse it ourselves, section 5.1).

Phase 3 dependency decision (t1527): exact versions are pinned in the workspace
manifest and the TUI package consumes the Ratatui/Crossterm pins. The current
workspace does not yet contain `ds-web`, so Axum is declared centrally for that
adapter and will enter `Cargo.lock` when its package is added; do not add Hyper
or another listener implementation directly. ds-core stays transport-neutral
and keeps the shared Tokio runtime dependency. Until full Rust web/TUI acceptance,
keep the installed Python web and TUI available as the boring fallback; do not
replace them with a hand-written HTTP parser or ship a production switch.

---

## 4. The compatibility contract (do not break these)

### 4.1 Wire protocol
`~/.config/dunkingsheep/dunkingsheep.sock` (mode 0600), newline-delimited JSON,
any number of requests per connection:

```
-> {"id": 1, "cmd": "task_next", "args": {...}, "self_pane_id": "%97", "service_token": "..."}
<- {"id": 1, "ok": true, "result": {...}}
<- {"id": 1, "ok": false, "error": "...", "retryable": true}
```
`id` is echoed; `self_pane_id` and `service_token` are optional; `retryable`
is present only when true. The peer pid comes from `SO_PEERCRED`. A request
with invalid JSON gets `{"id": null, "ok": false, "error": "invalid JSON"}`.
`shutdown` ends the loop after its reply. Keep `error` strings identical:
clients and tests match on them (`not permitted for role`, `requires a
superuser`, `no task with id`, `identity refused`, ...). The full list of
asserted strings is in `MANIFEST.md` section "Error strings".

### 4.2 The registry
`dunk_api.COMMANDS` (117 commands) is the single source of truth for the
socket (`dispatch`), the MCP tool schemas (`MCP_GROUPS`, 13 groups) and the
CLI. In Rust it is `registry.rs`: a `static COMMANDS: &[Command]` where
`Command { name, description, params: &[Param], required: &[&str], mcp: bool,
handler: Handler }` and `Param { name, kind: Str|Int|Bool|Array|Number, enum_: &[..], description }`.
`build.rs` renders the clap subcommands and the MCP `inputSchema` from it, so
a new command is one table row. The CLI's hand-written printers (52
`print_*` functions) are ported as `fn render_<name>(&Value) -> String` in
`ds-cli/src/render.rs`, snapshot-tested with `insta` against the Python output
captured by the harness.

### 4.3 Errors
```rust
#[derive(thiserror::Error, Debug)]
pub enum DunkError {
    #[error("{0}")] Refused(String),          // DunkError in Python: a named refusal, exit 2 in the CLI
    #[error("{0}")] Retryable(String),        // RetryableDunkError: retryable=true on the wire
    #[error("{0}")] Ambiguous(String),        // AmbiguousTarget
    #[error("identity refused: {0}")] Identity(String),
    #[error("daemon unavailable: {0}")] Unavailable(String),   // DaemonUnavailable (client side)
    #[error("database busy; retry the request")] Busy,        // sqlite SQLITE_BUSY/LOCKED -> retryable
}
```
The CLI maps `Refused|Ambiguous|Identity` to exit 2 with the message on
stderr, `Unavailable` to exit 1, a long-running "still working" to exit 75
(the timer units expect it).

### 4.4 Files and databases
Every path under `~/.config/dunkingsheep/` keeps its name and format
(`MANIFEST.md` "Files on disk" lists all 33). SQLite schemas are copied
verbatim from `MANIFEST.md` "Schemas"; migrations run the same `ALTER TABLE`
sequence. A Rust daemon must open the live databases of a Python daemon and
vice versa (that is the cutover and the rollback).

### 4.5 Identity and roles
Unchanged: `roles.json`, `service-tokens.json`, the allowlist tables in
`dunk_roles.py` (copied into `roles.rs` as `const` arrays), binding via
`/proc` ancestry. Port `dunk_callers.py` first (section "T03"), it is the
root of trust.

### 4.6 Systemd and the launcher
`~/.local/bin/dunkingsheep` is a shim that execs the release's binary; the
units' `ExecStart` lines do not change. A release is
`~/.local/share/dunkingsheep/releases/<sha>/` holding the Python tree today
and, after cutover, the Rust binary plus `packaging/`.

---

## 5. Concurrency and the hot paths

### 5.1 Terminal: tmux control mode replaces polling
Today every pane read is `tmux list-panes -a -F <14 fields>` (and
`capture-pane -p -e -S -N` for screens). Rust keeps **one control-mode client
per tmux server** (`tmux -C attach -t <session>` on a pty, or `tmux -C
new-session -d`): a line protocol where every command's output arrives between
`%begin`/`%end` and the server pushes `%output`, `%layout-change`,
`%window-add`, `%session-changed`, `%exit`, ... (`man tmux`, CONTROL MODE).

`terminal::control::Client`:
- `spawn()` starts `tmux -C` as a child with a pipe pair; a reader task parses
  lines into `Event`s; a writer takes `Request`s from an mpsc channel and
  matches replies by order (`%begin <time> <num>` / `%end <time> <num>`).
- `PaneTable`: `HashMap<PaneId, Pane>` refreshed from `list-panes -a -F
  <PANE_FORMAT>` on start and on any `%window-*`/`%session-*`/`%layout-change`
  event; `get_pane` is a map lookup; `list_panes_grouped` joins the window and
  session tables (same fields as the Python's `PANE_FORMAT`, `TAB_FORMAT`,
  `WORKSPACE_FORMAT`, copied verbatim).
- `capture(pane, lines, ansi)` sends `capture-pane -p [-e] -S -N -t %id`
  through the control client; the box-detection code (`dunk_core`'s typing
  guard, `dunk_stuck`) consumes the same text it does today.
- `send_text`, `send_keys`, `respawn-pane`, `resize-window`,
  `select-pane -T` are the same commands, sent through the client.
- Fallback: if control mode is unavailable (`DUNKINGSHEEP_TMUX_CONTROL=0` or
  the attach fails), `terminal::tmux` runs the same commands as subprocesses
  exactly as the Python does. The trait `Terminal` has both implementations;
  tests use `FakeTerminal` (port of `tests/fakes.py::FakeHerdr`).
- The 2-second role-pane cache (`_role_pane_cache`) becomes unnecessary with
  control mode; keep it behind the fallback only.

### 5.2 One writer actor per database
`db::Writer<S>`: a tokio task owning one `rusqlite::Connection` (WAL,
`busy_timeout 5000`), fed by an mpsc of `Box<dyn FnOnce(&mut Connection) ->
Result<Value> + Send>`; callers `await` the oneshot reply. Readers use a pool
of N read-only connections (`query_only`). Every Python method that did
`with self._lock: BEGIN IMMEDIATE ... commit` becomes one closure sent to the
writer, so a transaction never interleaves with another writer and never holds
a tokio worker thread. The claim transaction (`tasks::claim_next`) keeps its
`BEGIN IMMEDIATE` and its late-authorization re-read exactly.

### 5.3 The Flock, split
`dunk_core.py` is 9k lines because it holds the dunk scheduler, sends and
delivery, the inbox API, tasks API, roles, launches, state capture and the
loops. In Rust `Flock` is a struct of `Arc`s to the stores plus the actors; the
command handlers live in `flock/commands_<area>.rs` files that mirror the
Python method groups (the MANIFEST lists each method under its target file).
Background loops become tokio tasks: `task_reaper`, `task_push`,
`stuck_observer`, `dunk_scheduler` (one task per running dunk, sleeping until
its next fire), `outbox` (one task per pane with queued messages, same as the
per-pane thread today), `parent_watch` (die with the parent when started with
`--die-with-parent`).

### 5.4 What stays synchronous
`/proc` reads, file locks and sqlite are blocking; they run on
`tokio::task::spawn_blocking` or inside the writer actor, never on the
accept loop. The budget: a `status` request must answer in < 2 ms at 30
concurrent lanes (the Python measures 0.7 s to 9 s). The harness records
latency; T60 sets the gate.

---

## 6. Library-ification: what the port removes

Counts are from the current tree (`grep`, 2026-10-29):

| pattern in Python | sites | Rust home | lines after |
| --- | --- | --- | --- |
| atomic JSON write (`mkstemp`/`os.replace`/`chmod`) | 15 in 12 files | `atomic_json::{read, write}` | ~40 |
| per-thread sqlite connection + WAL pragmas | 3 stores, 45 `PRAGMA`/`BEGIN IMMEDIATE` sites | `db::{Writer, ReadPool}` | ~150 |
| retry/backoff loops (`attempt` counters) | 312 mentions in 19 files | `retry::with_backoff` + the client's connect retry | ~30 |
| `threading.Lock` around a dict | 24 in 14 files | ownership: the actor owns the state; `RwLock` only for the pane table and the registry | ~0 |
| `except Exception` swallow-and-log | 167 in 28 files | `Result` + `?`; the few deliberate "must not crash the request" sites keep `.unwrap_or_else(log)` and are named in the MANIFEST | ~0 |
| `time.time()` arithmetic | 145 in 27 files | `clock::{now_s, iso}` (one place to fake in tests) | ~20 |
| argparse (362 arguments, 156 subparsers) + 52 printers | 3,229-line CLI | generated from the registry; printers ported | ~900 |
| `h_*` handler shims (116) that re-pack args | `dunk_api.py` | `serde` deserialisation of `args` straight into a typed struct per command (`#[derive(Deserialize)]`) | ~0 |
| `/proc` parsing | 22 sites in 7 files | `proc.rs` (`stat`, `start`, `cmdline`, `ancestors`) | ~120 |
| `subprocess.run` of tmux/gh/git | 17 in 11 files | `terminal::control` (tmux) and `process::run` (gh, git, exporter) | ~60 |

Honest estimate: the daemon+CLI go from ~35k Python lines to **~18-22k Rust
lines** (Rust is more verbose per statement; the win is in the generated CLI,
the removed duplication and the deleted defensive code), and the test suite
ports roughly 1:1 (~30k lines; many assertions get shorter with `insta`).
Line count is not the goal; one implementation of each mechanism is.

Patterns worth keeping as they are (they are good): the command registry;
`_as_dunk_error` (becomes `?` + `From<TaskError> for DunkError`); the
append-only `task_events` as the source of metrics; the "record the
refusal reason" style (`_refusal`); the late-authorization re-read inside the
completion transaction; fakes for the terminal and the clock.

---

## 7. The differential harness (built first, used by every task)

`tools/diff_harness.py` (Python, stays Python):
1. `record`: a proxy on a second socket path that forwards every request to
   the Python daemon and appends `{request, response, latency_ms}` to
   `rust/fixtures/traffic.jsonl`. Run the fleet through it for a day; scrub
   tokens (`service_token`, `gh_token` results).
2. `replay --commands a,b --against <sock>`: sends the recorded requests to a
   daemon started on a **copy** of the state directory and diffs responses
   (ignoring the fields named in `fixtures/volatile.json`: timestamps, ids
   that the daemon allocates, `expires_at`). Prints a unified diff per
   mismatch and the latency ratio.
3. `golden`: the same, but against a frozen state-dir snapshot checked into
   `rust/fixtures/state/` (small: a few tasks, mail, roles, dunks) so a CI run
   needs no live daemon.

The Rust daemon starts with `dunkingsheep serve --state-dir <copy> --socket
<path>` so both can run side by side on one host.

---

## 8. Cutover and rollback

1. Shadow: the Rust daemon runs on `dunkingsheep-rs.sock` against a copy of
   the state dir; the harness replays live traffic to it hourly for two days;
   zero diffs on the phase-1 command set.
2. Switch: `tools/t514_rollout.sh stage --sha <rust-release> --apply`
   (the release dir contains the binary; the launcher shim is unchanged);
   `activate` restarts `dunkingsheep.service` on the Rust binary against the
   **real** state dir. Timers and the web desk notice nothing: same socket,
   same files, same CLI.
3. Rollback: `t514_rollout.sh rollback --apply` to the last Python release.
   Both daemons read and write the same schemas, so rollback is a restart.
4. Remove the Python daemon tree one release later; the TUI and the web desk
   (phase 3) follow on their own schedule.

---

## 9. Task shape (how `TASKS.md` is written)

Every task has: `id`, `title`, `depends_on`, `creates` (Rust files),
`ports` (Python files and the exact functions), `tests` (Python test files to
port), `acceptance` (tests pass, clippy clean, harness diff empty for the
named commands), `notes` (incident comments to keep, byte-identical strings).
A task is sized for one lane session (≤ 600 lines of Rust including tests).
Tasks are filed with `dunkingsheep task add --kind work -p <prio> --body
<the task text verbatim>` and depend on each other with `--depends-on`; the
queue orders them. Reviews are the fleet's normal review path.

---

## 10. Keeping the port in step

The owner's rule (ground truth `rust-parity`): until the cutover, every Python change to the daemon, CLI or schemas (`dunk_*.py`, `dunkingsheep`, `packaging/`) is mirrored in Rust, or the PR says why it has no Rust equivalent. Nobody remembers this by hand, so `tools/rust_parity.py` does:

1. It reads the commit named on the first line of `MANIFEST.md`, and diffs the Python tree between that commit and `HEAD` into **functions** (`fn:<file>::<Class.>name`, by AST, so comment edits are not changes), **registry commands** (`Command("name"`) and **schema columns** (`CREATE TABLE` bodies and `_migrate` tuples).
2. It checks each added or changed item against `rust/crates` **by name**. No counterpart, or a counterpart older than the Python change (STALE), is reported; a removed item the Rust tree still has is reported too. A leading underscore is ignored (`_gate_unread` matches `gate_unread`).
3. `--warn` prints GitHub annotations (the `rust-parity` workflow, warn-only, never fails a PR); `--file-tasks` files one `Rust parity: <module>` task per module with unported items, deduped by title (the daily `dunkingsheep-parity.timer`); `--check` exits 1 for scripts.

What it does **not** know: whether the Rust behaviour is equal. A name match means a counterpart exists; the differential harness (section 7) is the judge. When you port an item, name the Rust function after the Python one (rule 2 of section 0) and the watcher clears it. When you regenerate the manifest (`tools/gen_manifest.py`), the base commit moves and the list resets.
