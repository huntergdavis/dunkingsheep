# Per-caller CPU from /proc

`dunkingsheep load` samples the registered callers' CPU from `/proc`, with no
subprocesses and no model. Identity is `(pid, starttime)`; the sample is the
caller's **process tree**, because a lane's real work happens in children.

## The guard

Before attributing anything, `/proc/<pid>/stat` field 22 (starttime) must equal
the stored `caller_start`. A recycled pid produces **no sample**, and is listed
in `skipped` with `pid reused (starttime differs)`. A missing process is
`process gone`. A wrong attribution is worse than a missing one.

## The tree (and its limit)

`tree_of()` walks one `ppid` map built from `/proc/*/stat` and returns the caller
plus every descendant. The cost is one directory walk plus N small reads per
interval. **Known limit:** a child that reparents to `init` escapes the tree;
that work lands in the `unattributed` remainder instead of being guessed onto a
lane.

## The number that has to sum

`collect()` returns `host_load1`, `attributed_cores` (the sum of the caller
percentages, in cores) and `unattributed_cores = load1 - attributed`. The
remainder is printed, never dropped: if the per-agent figures sum to 15% while
load is 20, the report says `unattributed 5%`.

## GPU

This host is an Intel UHD 620 (Kaby Lake-R) with no `nvidia-smi`, so there is no
per-process GPU counter. The sampler checks DRM fdinfo for `drm-engine-*` lines
and reports what it finds; today the cell is **null** with the reason
`no per-process GPU counter on this hardware (Intel UHD 620)`. It never turns a
global utilisation into a per-agent percentage.

## Fields

Per caller: `name`, `pid`, `cpu_pct` (window mean, % of one core),
`peak_cpu_pct`, `rss_mb`, `process_count`, `gpu_pct` (`{value, reason}`), all in
the same cell discipline as the ops snapshot.
