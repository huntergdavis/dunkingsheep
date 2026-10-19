# Self-sustaining daemon (t12)

The daemon must survive a kill, repair itself without a human, and never put an
LLM in the control loop. Three pieces make that true, and each has a fixed,
constant threshold — no model is consulted.

## 1. systemd owns the process (`packaging/systemd/`)

| Unit | Purpose |
| --- | --- |
| `dunkingsheep.service` | `ExecStart=dunkingsheep serve`, `Restart=always`, `RestartSec=2`, `StartLimitIntervalSec=0` |
| `dunkingsheep-health.service` + `.timer` | every 10 min: `dunkingsheep --no-start-daemon health --repair --write` |
| `dunkingsheep-deepcheck.service` + `.timer` | weekly: `dunkingsheep --no-start-daemon health --deep --repair --write` |

Install:

```sh
cp packaging/systemd/* ~/.config/systemd/user/
systemctl --user daemon-reload
systemctl --user enable --now dunkingsheep.service \
  dunkingsheep-health.timer dunkingsheep-deepcheck.timer
loginctl enable-linger "$USER"   # keep running after logout
```

**One owner.** When the unit file is installed, the client never spawns a second
`serve`; `FlockClient.start_daemon` asks `systemctl --user start` instead, and
`run_daemon` refuses to bind the default socket without systemd's
`INVOCATION_ID`. Both are deterministic checks, and together they make a
socket-stealing race impossible. On a machine without the unit, the client
spawns a detached `serve` exactly as before.

## 2. Deterministic health self-check (`dunk_health.py`)

`dunkingsheep health [--repair] [--deep]` (MCP tool `health`) returns
`{ok, problems, repairs, checks}`:

| Check | Threshold | Repair (`--repair`) |
| --- | --- | --- |
| memory DB reachable, `ok` flag | `status.ok` | — |
| SQLite integrity | `ok (shallow)` by default; `PRAGMA quick_check` only with `--deep` (it scans the whole DB, ~18 s at 277 MB) | — |
| stale ingest leases | expired lease or a `partial` source left by a restart | `recover_leases()` + `requeue_partial()` |
| WAL size | `> 64 MiB` (`WAL_WARN_BYTES`) | `checkpoint()` (PASSIVE, never blocks a reader) |
| free disk | `< 512 MiB` (`DISK_MIN_FREE_BYTES`) | — |
| task counts | always reported | — |

`ok` is `problems == []`. A repaired condition moves from `problems` to
`repairs`, so a green report means nothing is left to fix. The daemon also runs
`recover_leases()` + `requeue_partial()` on its existing maintenance tick, so a
crashed ingest worker is requeued within one tick even if the timer is off.

The health timer runs the report with `--write`, which writes
`~/.config/dunkingsheep/health.json` for ops tooling. It uses
`--no-start-daemon`: if the daemon is down the timer fails and systemd restarts
the unit — a down daemon must never be masked by a client auto-starting one.

## 3. Kill/restart proof (`tools/self_heal_test.sh`)

```sh
bash tools/self_heal_test.sh
```

1. daemon answers; record pid and chunk count
2. `kill -9` the main pid (no clean shutdown, stale socket)
3. wait up to 15 s for the supervisor to bring it back
4. assert a new pid, unchanged chunk count, `health ok`, and a working recall

Run on 2026-10-18 against 262,299 chunks:

```
1. status            pid=1858974 chunks=262299
2. kill -9 1858974
3. wait up to 15s for restart
   pid=1860650 (new)
4. chunks=262299 (unchanged), health ok, recall ok
PASS: kill -9 -> automatic restart -> no data loss
```

`systemctl --user show -p MainPID dunkingsheep.service` equals the socket pid
after the test (`active`, one process), so no stray daemon was left behind.
