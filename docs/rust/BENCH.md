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

| Revision | Host | Panes | Warmup | Samples | p50 | p95 | p99 | Max |
| --- | --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| Pending exact-head benchmark run | | 1,000 | 1,000 | 500 × 20 | | | | |

The benchmark prints the host name, OS, architecture, warmup count, sample
count, and timings so a result can be copied here with the exact revision and
machine details. This synthetic table measurement does not establish the
separate release-build performance gate.
