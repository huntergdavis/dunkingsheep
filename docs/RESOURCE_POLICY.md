# Per-agent resource policy

Owner #1277. Measurement is the #1252 sampler (`dunk_load`), so this never
samples `/proc` twice. `dunk_policy` turns a sample into a finding and, only
when the policy explicitly allows it, a kill decision.

## Alerts (safe, default)

A per-agent policy names thresholds and the window each must hold over:

```json
{"cpu_pct_over": 300, "cpu_window_s": 600,
 "rss_mb_over": 1500, "rss_window_s": 300,
 "wall_minutes_over": 90}
```

A threshold without its window is a **policy error**, not a guess. On breach:
one finding, one inbox line, one digest row, with a per agent+rule cooldown.

## Auto-kill (conservative, default OFF)

Every clause is load-bearing:

1. **Default off.** Killing needs `"kill": true` on that agent's policy.
2. **Attributed only.** The pid must be a `(pid, starttime)` the daemon
   registered; a bare process name or an unrecorded pid is refused.
3. **TERM, grace (30 s), then KILL**, and both the decision and the outcome are
   logged with the triggering sample.
4. **Dry run first, for at least a day.** A `dry_run` policy logs
   `would-kill <agent> (pid N, 412% cpu)` and signals nothing. The act is
   enabled only after the dry-run output has been reviewed.
5. **Deny list wins.** The VP pane, the owner's shell, the daemon, systemd
   services, and anything whose cwd is not a workspace are refused by
   evaluation, not by luck.
6. **Heavy work is expected.** A compile saturating eight cores is not a
   runaway lane. Policies are **per lane**, never global; a lane we asked to
   compile gets a policy that permits it.

## Admin surface

Read-only vs read-write per MCP client, enable/disable the MCP endpoint, and one
"pause all agent writes" switch. Those are the controls with a safety argument;
anything else the owner meant, we ask before building.

## Why evaluation, not observation

A denied pid must be refused because the policy says so, not because its
`/proc` entry happened to be missing. `denied()` and `kill_decision()` are pure
enough to test that directly, and `tests/test_dunk_policy.py` does.
