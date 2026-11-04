# Rust service benchmarks

## 1,000-pane control-mode list

Run the named benchmark with:

```sh
cd rust
cargo bench -p ds-core --bench service -- bench_list_panes_1000
```

The harness parses 1,000 synthetic rows in the same 14-field format used by
`tmux list-panes -F PANE_FORMAT` into `terminal::control::PaneTable`, then
measures `PaneTable::list_panes` cloning the in-memory table. Input generation
and parsing happen outside the timed section. It warms the method 1,000 times,
then records 500 samples of 20 calls and reports p50, p95, p99, and max per-call
time. The performance target is under 50 ms per call.

### Measurement

| Revision | Host | Panes | Run outcome | Measured samples | p50 | p95 | p99 | Max |
| --- | --- | ---: | --- | ---: | --- | --- | --- | --- |
| `974530cee47853513c0efddcf2c1166a018d391a` | `cell`, Linux 7.0.0 x86_64 | 1,000 | **NOT RUN**: three attempts stopped after 120 s during dependency compilation | 0 (planned 500 × 20) | NOT MEASURED | NOT MEASURED | NOT MEASURED | NOT MEASURED |

The benchmark prints the host name, OS, architecture, warmup count, sample
count, and timings so a completed result can be copied here with the exact
revision and machine details. In the recorded row, all three local `cargo
bench` attempts stopped during dependency compilation before the executable
started, so no warmup or measured samples ran. This synthetic table measurement
does not establish the separate release-build performance gate.

### CI measurement (2026-11-02)

[Run 41482119996](https://github.com/huntergdavis/dunkingsheep/actions/runs/41482119996)
executed the optimized benchmark with Rust 1.99.1 on GitHub runner
`runnervm48cm3` (Linux 7.0.0-1017-azure, x86_64). Tested merge revision:
`de1c777255443f4b1c3a7b8624a5861d22531985`, combining benchmark head
`00b9fcd6b1eefc3362a227e79346080445caae06` with base `52caad005a9b5bd6eef9141057ac254361b3b495`.

After 1,000 warmup calls, 500 samples of 20 calls (10,000 measured calls)
produced p50 **334,994 ns**, p95 **339,475 ns**, p99 **342,765 ns**, and max
**348,491 ns** per call. The synthetic median is below 50 ms. The downloadable
`service-benchmark` artifact records the revision, compiler, host and output.

These timings measure only cloning the preloaded synthetic pane table. They do
not measure tmux I/O, live pane discovery, socket RPC, or end-to-end daemon
latency, and do not establish the separate production performance gate.

## Real socket RPCs

The existing `service` target also runs these named benchmarks:

```sh
cd rust
cargo bench -p ds-core --bench service -- bench_status_30_clients
cargo bench -p ds-core --bench service -- bench_inbox_read
cargo bench -p ds-core --bench service -- bench_task_next
```

Each uses an isolated temporary Unix socket, `FlockServer`, the canonical
`RuntimeServices::production_dispatcher`, real task/inbox SQLite stores and
registered worker role. Only terminal discovery uses a controlled one-pane
fixture. Its pane root is the live process returned by the production caller
resolver. Every connection checks `whoami` for the worker role and successful
kernel-peer identity binding before warmup. No live daemon or terminal is used.
Temporary fixture data lives under the working directory (override with
`DS_BENCH_DIR` to select a shorter workspace path for the Unix socket limit).
No additional Cargo target or dependency is introduced.

All timings include request serialization, socket write/read, the real handler
and response decoding. Socket connection, identity preflight, fixture setup,
semantic assertions and cleanup are outside timing. Clients keep their sockets
open; these are warm persistent-connection RPC latencies, not connection setup
or subprocess CLI latencies. Each client performs 20 warmup calls and 200
measured calls. Percentiles use sorted observations at index `(n - 1) * p / 100`.

- `bench_status_30_clients`: 30 established clients start together on separate
  threads, each running requests serially. Reports pooled per-request latency
  across 6,000 successful measured calls, not batch throughput. Every response
  must contain the fixture version, process PID and actual task totals.
- `bench_inbox_read`: one client consumes one newly seeded unread message per
  call with default options (including task queues). Seeding is outside timing;
  every response must return the exact message ID/body and mark one message read.
  The canonical store must then report zero unread. Read history grows through
  the 220 calls; the benchmark does not replace a read with an empty/peek result.
- `bench_task_next`: one client repeatedly claims one real assigned task and
  checks its ID and `in_progress` status. The same authenticated caller releases
  it to `open` outside timing after every call. Task event history grows through
  220 successful claims; empty/refused claims abort the run.

The existing p50 requirements are **CEILING** values: status **<2 ms**, inbox
read **<5 ms**, task next **<10 ms**. Output labels each observed comparison
PASS or FAIL without weakening the threshold; semantic failure aborts instead
of producing valid samples. Timing failures remain reported measurements, not
claims that the harness itself malfunctioned. Host, CPU, load before/after,
revision, counts, p50/p95/p99 and max are printed. The service benchmark workflow
runs all three names plus the existing 1,000-pane case and uploads the full log.

All benchmark-owned client and accept-loop threads are joined; clients finish
responses and close sockets before service shutdown. `FlockServer` currently
keeps no public join handles for its internal connection threads. These runs
therefore do **not** verify the separate production request-thread drain gate.
They also do **not** satisfy parent t822's final accepted Phase-1 release-build
comparison, which remains **NOT VERIFIED**.

### RPC measurements

Pending exact-head CI execution. No local cold build has been attempted and no
latency or CEILING result is claimed yet.
