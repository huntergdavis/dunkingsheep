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
