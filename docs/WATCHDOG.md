# Exception watchdog

**Review criterion:** the escalation behaviour below is reviewed against [WATCHDOG_ESCALATION_SPEC.md](WATCHDOG_ESCALATION_SPEC.md), written before this implementation. Where the code and the spec disagree, the code changes or the spec is argued in writing. Section 6 of the spec records the VP's rulings.

The watchdog replaces the VP's polling loop with deterministic alerts. It reads
the stores Dunking Sheep already keeps, applies fixed rules, and sends **one
digest per run** to the VP inbox. It pages the owner only for security, outage
and money, only after a hold, and a page counts only once the forwarding path
confirms it. Owner paging ships **disabled**, and stays disabled until the
Tier 2 re-review (VP i19817).

## Hard constraints (owner, VP i19781)

1. **Deterministic and configurable.**
   - Every rule is a fixed predicate over read-only SQL and JSON reads.
   - Every threshold, window, severity, class, hold, cap, cooldown and recipient
     lives in one config file.
   - The same stores, config, state and `--now` produce the same findings in the
     same order. Every time-windowed query is bounded above by `now`, so a past
     `--now` never sees the future.
2. **No LLM at runtime, ever.** `tests/test_dunk_watchdog.py::NoModelTests` fails
   the build on any import outside a short allowlist, any shell-out, or any model
   vendor or tool name. A second test proves the guard catches such a call.
3. **Not a dashboard.** Output is spec-format lines (or JSON), the inbox digest,
   and a derived `needs_attention` count. There is no UI, no HTTP server and no
   web page; visualising is the metrics collector's job.

## Channels

- **VP** is the dunkingsheep inbox. Use it for watchdog coordination alerts. Alerts
  arrive from sender `watchdog` (see below).
- **OWNER** is the `owner` mailbox in `inbox.db`, and the phone is one of its
  transports (FLAT_FLEET.md 7.2). The bridge no longer forwards briefs:
  `send_briefs` survives only as a name for callers that still say it, and it now
  sends the owner outbox, not briefs. What reaches Telegram is (a) replies whose question
  arrived on Telegram, and (b) pages the watchdog writes into the desk store,
  classified security / outage / money and capped at three per hour. A page that
  the cap holds is retried on a later tick - the watermark advances only for a
  page that was delivered - and nothing else (no reports, no digests, no web
  echoes) is sent to the phone. Do not put agent-only coordination into pages.
- **Delivery.** A page is DELIVERED only when the bridge's non-secret status
  file `vp-of-agentry/data/telegram-status.json`
  `{"channel": "cos", "last_brief_id": int, "updated_at": float}` shows `last_brief_id >= the
  posted brief id` and both the status and pending page match the route selected by the shared reader. Until then the page is pending: it is never re-posted and no
  cooldown starts.
  - A page still unconfirmed after `confirm_within_minutes` is reported to VP as
    UNDELIVERED.
  - The watchdog never opens `telegram.json`, which holds the bot token.
- **Posting is not wired.** Posting the brief needs the desk API token, and how
  the watchdog may hold it is undecided. `post` therefore fails toward not
  paging: nothing is posted, and VP gets a self error.

## Inputs (read-only; no schema changes)

| Store | Used by |
| --- | --- |
| `tasks.db` (`tasks`, `task_leases`, `task_events`) | task and lane rules; task text for the secret scan |
| `inbox.db` (`messages`) | inbox rules, routing history, pane activity; message bodies **only** for the secret scan |
| `metrics.db` (`events`) | skip-loop rule |
| `dunks.json`, `health.json` | dunk rules (and dunk text for the secret scan), health rule |
| host sample (`os.statvfs`, `os.getloadavg`) | host rules |
| herdr pane snapshot (`herdr pane list`) | `inbox.unread_dead`, the lane rules, `require_live_pane` |
| `/tmp/poolrad-heavy.*` slot mtimes | `host.heavy_slot_stale` |
| `telegram-status.json` | owner delivery confirmation only |

Evidence carries ids, counts, ages and hashes. It never carries message or task
text.

## Rules

Every rule has these keys, plus the parameters in the table below:
- `enabled`;
- `severity` (`info`, `warning` or `critical`);
- `class` (`security`, `outage`, `money`, `ops` or `project`);
- `cooldown_minutes` (> 0);
- `must_hold_runs` (>= 1, applied to owner pages only).

The validator refuses an owner class on any rule outside `OWNER_ELIGIBLE_RULES`
(spec section 1).

| Rule id | Fires when | Parameters | Shipped |
| --- | --- | --- | --- |
| `task.blocked_stale` | a blocked task has had no update or event for at least the threshold | `threshold_minutes` | warning / ops |
| `task.lease_stale` | a lease is expired but not yet reaped, or was never renewed for at least N min, or a **fresh** reap event (`expired`/`pane_gone`) falls in the fresh window. A reap event is reported once ever (`once=True`), not again after a cooldown: history is not news. | `never_renewed_minutes`, `lookback_minutes`, `fresh_minutes` | warning / ops |
| `dunk.skip_loop` | the longest consecutive run of `dunk.skipped` in the lookback is at least N. Keyed by the run's first skip, so a growing loop is one incident | `consecutive_skips`, `lookback_hours` | warning / ops |
| `dunk.overdue` | a running dunk is at least N intervals past `next_send_at`, or has no scheduled send | `missed_intervals` | warning / ops |
| `inbox.undeliverable` | a mailbox's undeliverable backlog is at least N (no age limit) | `min_messages` | warning / ops |
| `inbox.unread_live` | a live pane's mailbox has at least N queued messages older than the age | `age_minutes`, `min_messages` | warning / ops |
| `inbox.unread_dead` | queued mail for a pane that no longer exists | `min_messages` | warning / ops |
| `inbox.ambiguous_sender` | a sender is a mailbox name that routes to a (live) pane, not a pane id. Samples are the first `sample_ids` by time (the sample ships 30) | `lookback_hours`, `sample_ids`, `require_live_pane` | info / ops |
| `lane.working_without_task` | a pane is working, holds no task, and has no task event in N min | `quiet_minutes` | info / ops |
| `lane.idle_holding_task` | a pane holds a task while idle, with no task, send or read activity in N min | `quiet_minutes`, `idle_statuses`, `holding_statuses` | warning / ops |
| `host.load` | per-CPU **load5** at or above a band fitted to our own history (3.0/cpu warning, 3.5/cpu critical, sustained 3 runs). The keys are named `load1_*` for compatibility; the value is load5 | `load1_per_cpu`, `critical_load1_per_cpu`, `critical_hold_runs` | warning / ops |
| `host.tmp` | used fraction is at least 0.70 (warning) or at least 0.90 (critical), free bytes are at or below the floor, or the inode fraction is at or above the limit | `max_used_fraction`, `critical_used_fraction`, `min_free_bytes`, `max_inode_fraction`, `path` | warning / ops |
| `host.disk_full` | /tmp or / is at least 98% full | `max_used_fraction`, `paths` | **critical / outage, hold 2** |
| `host.heavy_slot_stale` | a heavy-job semaphore slot is older than N min (no holder pid exists) | `directory`, `prefix`, `max_age_minutes` | warning / ops |
| `security.secret_text` | a credential-shaped line appears in inbox, task or dunk text, re-verified on a second read; allowlisted line hashes are skipped | `patterns`, `allowlist_sha256`, `lookback_hours` | **critical / security** |
| `money.spend_projection` | never: there is no metering source. It cannot be enabled without `source` and `cap`, and always prints SKIPPED | `source`, `cap` | disabled / money |
| `resource.would_kill` | one info line per lane that would trip a **resource policy** while holding a leased task. Never signals; the point is that our threshold is probably wrong, not the lane | `policies_path`, `state_path`, `require_task` | info / ops |
| `health.failed` | `health.json` is stale, or reports problems | `stale_minutes` | critical / ops |
| `watchdog.self_error` | anything the watchdog could not do this run: a rule error, missing or corrupt state, or an owner post or confirmation problem. One finding per run | (none) | VP only |
| `watchdog.deadman` | the heartbeat is missing or older than 3x the cadence (separate process) | CLI flags | VP only |

## `host.load`: a band fitted to our own history (2026-10-22)

`host.load` warns at **3.0/cpu** and is critical at **3.5/cpu**, judged on
**`load5`**, sustained 3 runs. The number now comes from 461 recorded samples
over 8.3 hours of this machine doing the work we assigned it, not from a guess.

### The fit

Ordinary least squares on `load5 / cpus`:

```text
load5/cpu = 0.49 + 0.190*busy + 0.336*heavy          n = 461, R2 = 0.52, RMS = 0.33 load/cpu
load5/cpu = 0.45 + 0.240*busy                        n = 461, R2 = 0.49, RMS = 0.34
load5/cpu = 0.40 + 0.095*busy + 0.007*heavy + 0.022*psi60   n = 461, R2 = 0.76, RMS = 0.24
```

`busy` = panes with `agent_status working`, `heavy` = live heavy-slot holders,
`psi60` = CPU pressure stall over 60 s. **`live` is not in the model**: it was
27 in every one of the 461 samples, so its coefficient is unidentifiable from
this data - the matrix is singular. That is also why the earlier lane-reserve
shape (`0.55 CPU/lane`) could never have been estimated from a week of samples.

The third model fits best but was not adopted: pressure stall is a *consequence*
of load, not a count of our work, so a threshold on it would page on a symptom
we already measure directly. It is recorded here because a future reader should
know the fitted shape is only half the story.

### What normal looks like (compare a reading in front of you; no arithmetic)

**Predicted** is the model, so nobody has to compute it. **Observed** is what the
machine actually did in that regime; where the two differ, the model is the
lower bound of what to expect.

| busy lanes | heavy jobs | predicted load5/cpu | predicted load5 (8 cpus) | observed median | p90 | max | samples |
|---|---|---|---|---|---|---|---|
| 0 | 0 | **0.49** | 3.9 | 0.35 | 0.53 | 1.77 | 196 |
| 1 | 0 | **0.68** | 5.4 | 0.76 | 1.58 | 2.46 | 91 |
| 2 | 0 | **0.87** | 7.0 | 0.80 | 1.31 | 2.46 | 124 |
| 4 | 1 | **1.59** | 12.7 | 2.31 | 2.69 | 2.69 | 2 |
| 5 | 2 | **2.11** | 16.9 | 2.20 | 2.76 | 2.76 | 6 |
| >= 4 | any | 1.59 - 2.11 | 12.7 - 16.9 | 1.93 | 2.50 | **2.79** | 27 |

Read it like this: four lanes plus a compile *predicts* 1.59/cpu and actually
measured 2.31-2.69, i.e. the model understates the top of the range by about
0.7/cpu - roughly two fit residuals. That is why the band is set from the
observed envelope and not from the prediction: a threshold derived from the model
alone would sit at 1.6 and page on every compile. The machine reached
**2.79/cpu** while doing assigned work (two compiles, an assembler and an
objdump), so any threshold at or below 2.8 pages on our own productivity.

### Finding: per-lane cost is not measurable while the lane count is constant

`live` was 27 in all 461 samples, so the design matrix is **singular**: the
coefficient on `live` does not exist as an estimate, it is not merely imprecise.
Consequence, stated plainly because it cost a week of guessing:

> **Per-lane cost is not measurable while the lane count is constant. The model
> uses busy lanes and heavy jobs only.**

The earlier lane-reserve shape (`0.55 CPU/lane`, `warning = lane_reserve *
lanes / cpus`) could never have been estimated from this history by any method -
the data contains no variation in the lane count. Estimating a per-lane cost
needs a day in which panes are opened and closed, or a deliberate experiment that
varies them. Until then `live` stays out of the model, and any future proposal to
tune a per-lane constant should be answered with this paragraph.

### Why the band is not a quantile

Percentiles do not help here: p95 was 1.84 and p99 was 2.46, yet a *legitimate*
reading of 2.65 fired CRITICAL at the old 2.6. A quantile of a mostly-idle
history is below the peak of a working day. The band is therefore set from the
**observed maximum of legitimate work plus a margin**:

```text
warning  3.0/cpu  = observed maximum 2.79 + 0.21   (~0.6 sigma)
critical 3.5/cpu  = warning + 0.5                  (~1.5 sigma, and 2 sigma above the maximum)
```

Zero of the 461 samples reached 3.0. The first warning is therefore genuinely
novel, which is the property a pager needs; the cost is that a runaway sitting
between 2.8 and 3.0 will not page. With `must_hold_runs: 3` and a 30-minute
cooldown, a sustained runaway climbs well past 3.5 within minutes of starting,
so that gap is small in practice.

### What the fit cannot tell you

- **`load5` is a five-minute average; `busy`/`heavy` are instantaneous.** The
  residual (RMS 0.33/cpu) is largely that mismatch. A lane that starts and
  finishes inside the window barely moves load5; four lanes that run for an
  hour move it a lot. This is why the model explains 52% of the variance and
  why the band is set from the envelope rather than the prediction.
- **Only 27 samples had 4+ busy lanes** and only 2 had exactly 4 busy + 1 heavy.
  The top of the table is a small-sample claim and should be treated as such.
- **`live` was constant.** A day where panes are opened and closed would allow a
  per-pane term; until then it stays out of the model.

The alert keeps every ingredient, so it is diagnosable from the message alone:

```text
load5 26.4 = 3.30/cpu, band 3.0-3.5/cpu (fitted 2026-10-22), 27 live / 5 busy / 2 heavy
```

The config keys are still named `load1_per_cpu` / `critical_load1_per_cpu` for
compatibility with existing config files; the number they hold is per-CPU
**load5**.

## `resource.would_kill`: the threshold-wrong signal, in the digest

A resource policy is a guess until a lane that is legitimately working crosses
one. This rule reports exactly that, and nothing else:

- It samples the same way `dunkingsheep load --policies` does, from the caller
  identity recorded on each task lease, but keeps **its own state file**, so the
  watchdog and the CLI never corrupt each other's sample window.
- It **never signals**. `dry_run` and `kill` are recorded in the evidence; the
  finding says what *would* have happened.
- It fires **only for a lane holding an in-progress lease** (`require_task`). An
  idle lane crossing a cpu limit is not evidence that the limit is wrong; a lane
  that is busy on t6 and crosses it is.
- No policies file means no findings, not an error: there is nothing to cross.
  An unreadable policies file raises a config error, which is a self error, not
  a silent pass.

The line reads like this:

```text
w8:p8 would be flagged by resource policy while working on t6: advance strict C toward the 815-body ceiling
  threshold=policy limits  measured=cpu_pct_over 96.0% > 80% 60s  window=sample window
```

Severity is **info** on purpose: the owner asked for this in the digest, because
a lane being expensive is a fact about our thresholds until proven otherwise.

## `dunk.skip_loop`: a running streak, not a remembered one

d52 paged with "33 consecutive skips, cumulative 52" while **its last skip was 76
hours old** and its target run had ended days earlier. The streak was real; the
alarm was not. Two corrections, both about the same habit - an alert must describe
a condition that is happening:

1. **`fresh_minutes` (default 120).** A streak whose most recent skip is older than
   this is history and is not reported. The lookback still bounds the search; it no
   longer decides whether the finding is current.
2. **The skip counter resets with the run.** `skip_count` is now the *current*
   run's skips and goes back to zero on a delivered send; the lifetime number is
   `total_skips`, reported separately as
   `cumulative_skip_count_lifetime`. A counter that only grows eventually fires on
   its own history rather than on a current condition.

A dunk aimed at a **retired pane** is reported once, at info, as obsolete:

```text
dunk d52 targets retired pane w9:p9 (superseded-by:w9:p10) - its streak is not a condition
```

It is `once=True`, so it says this one time and stops, rather than accumulating a
streak that eventually pages. The alert for a live loop now names when it last
happened: `dunk d9 skipped 5 times in a row (last 2 min ago)`.

## `inbox.unread_live`: state first, then age

**A working lane's unread mail is a queue; an idle lane's is a miss.** par-c was
authoring a body for four hours with two unread messages, and the rule paged about
it: reading mail mid-authoring is a context switch the lane should not make, and the
mail would be read when the body landed. Nothing was wrong.

So the rule now requires the lane's **own state** to be idle (`idle_statuses`,
default `idle` and `done`) *as well as* the age threshold:

```text
inbox.unread_live = mail older than age_minutes
                    AND its pane's agent_status is idle/done
                    AND the mailbox is not retired
```

An **unknown** status does not page either: we cannot say a lane missed mail we
cannot see it holding. The exception is the degraded mode where the pane source is
disabled entirely - then the rule keeps its old behaviour and says so in the
evidence (`pane_liveness: unknown (pane source disabled)`), because a silent rule is
worse than one that explains itself.

The number now means something the owner can act on: **idle lanes with unread mail
older than N** is the count of lanes we failed to reach, and it should be zero.
"Working lanes with unread mail" is throughput.

This is the fourth rule tonight with the same shape - retired pane, dead session,
placeholder-only box, and now this: **state first, age second.** Age is not
liveness, and it is not idleness either.

## Before you model a number, check the recorder

Three recorders threw data away tonight: `pane_samples` only recorded the
poolrad lanes, the host-watch history kept only two samples (`history[-2:]`),
and a case-sensitive grep "proved" absence while the word sat in a subtitle
with a capital letter. Two samples were enough for the load LEVEL and useless
for a FIT; the recorder now keeps a bounded 10-day history. A fit also needs a
minimum sample count (30) across at least two heavy/busy regimes, and must
report the residual error - otherwise it is the "load per busy lane ~12" guess
again.

## Config (`~/.config/dunkingsheep/watchdog.json`)

The schema is `dunkingsheep-watchdog/2`. **Every key is required, unknown keys
are refused, and thresholds, windows, holds, caps and cooldowns must be > 0.**
A missing or invalid file:
- exits 2 with `refusing to start: CONFIG: <dotted.key>: <reason>`;
- writes the same text to `--error-file` for the dead-man to carry to the VP;
- sends nothing.

`packaging/watchdog.example.json` is the full sample. It ships quiet:
`enabled: false`, `dry_run: true`, owner disabled.

```text
schema, enabled, dry_run          live alerts need enabled=true AND dry_run=false AND no --dry-run flag;
                                  --now is refused on a live run (it would backdate state and the heartbeat)
cadence_minutes                   > 0; the dead-man unit uses 3x it
min_run_spacing_seconds           > 0; a run closer than this to the last counted one does not advance a hold
max_alerts_per_run                VP cap; the overflow is one "N further finding(s) suppressed" line
state_path, heartbeat_path        own files, written atomically; the heartbeat only after a successful live run
state_retention_days              forget dedupe keys unseen for this long
stores.config_dir                 tasks.db / inbox.db / metrics.db / dunks.json / health.json
panes.source, pane_id_pattern     "herdr" | "none"; the regex that separates pane ids from mailbox names
escalation.vp                     {mailbox, min_severity}
escalation.owner                  {enabled, channel "desk-brief", classes ⊆ security/outage/money, min_severity,
                                   approved_by (required when enabled), max_pages_per_hour (3),
                                   confirm_within_minutes, confirm_path}
rules.<id>                        {enabled, severity, class, cooldown_minutes, must_hold_runs, ...parameters}
```

## Fail toward not paging (spec section 3)

Each of the following leaves the OWNER queue untouched and tells VP once:
- a bad config (the process never runs);
- missing or corrupt state (the whole run's owner pages are suppressed, and the
  message names the **state path**);
- any rule error (owner pages suppressed for the run);
- an owner post or confirmation error (nothing is recorded as sent).

Every suppression check runs **before** anything is posted, because posting a
brief is the irrevocable "marked for forwarding" step. If the VP digest itself
cannot be delivered, the run prints `DELIVERY FAILED (source=watchdog-self)`,
exits 1, and writes no state or heartbeat, so the dead-man reports it. It never
retries through the inbox.

## Dry-run contract (spec section 4)

```text
WOULD-PAGE OWNER rule=<id> class=<c> severity=<s> source=<watchdog-rule> threshold=<v> measured=<v>
                 window=<w; held k/N runs> dedupe=<rule|key>
                 suppressed=<no|hold|dedupe|global-cap|state-loss|self-error|disabled|below-min-severity>
                 evidence=<json>
WOULD-NOTIFY VP  ... suppressed=<no|dedupe|rate|below-min-severity> ...
SKIPPED rule=<id> error=<raw>
RESOLVED VP|OWNER dedupe=<rule|key>
SELF source=watchdog-self <problem>
```

## Commands

```sh
dunkingsheep watchdog --dry-run [--json] [--config PATH] [--now UNIX_TIME]
dunkingsheep watchdog --replay --from T [--to T] --step MINUTES [--json]   # history-backed rules only
dunkingsheep watchdog                                       # live, only if the config allows it
dunkingsheep watchdog-deadman --cadence-minutes 10 --multiplier 3 --cooldown-minutes 60 \
    --to "vp of agentry" [--heartbeat P] [--state P] [--error-file P] [--status P] [--dry-run] [--json]
```

Both are local commands. They read the stores directly and never start the
daemon; only a live send talks to it.

## Dead man (quis custodiet)

`dunk_watchdog_deadman.py` runs in its own systemd unit and timer, and never
imports the watchdog or reads its config.
- **Alert.** If the heartbeat is missing or older than `multiplier x cadence`,
  it sends one `watchdog.deadman` alert to the VP, once per its own cooldown.
  The alert includes the raw config refusal from the error file, if one is
  present.
- **Delivery never depends on the daemon it may be reporting** (VP i19837,
  w8:p8 i19841):
  1. **Daemon path check, before anything is sent** (VP i19851, w8:p8
     i19854). A misconfigured path raises the same `FileNotFoundError` as a
     dead daemon. So `daemon_path_problem()` first requires that the path
     (`--socket`, resolved like `dunk_client`: `$DUNKINGSHEEP_SOCKET` or
     `~/.config/dunkingsheep/dunkingsheep.sock`) is non-empty and that its
     parent exists, is a directory, and is the expected one
     (`--expected-socket-dir`, default `~/.config/dunkingsheep`). If any of
     these fails, it is a **CONFIG refusal**: nothing is sent, no direct row is
     written, the status file records `cause: config_bad`, and the dead-man
     exits 2.
     A daemon on a custom socket outside `~/.config/dunkingsheep` needs BOTH
     `--socket` and `--expected-socket-dir` in the dead-man unit; otherwise the
     dead-man reports `config_bad` (deliberately: it will not guess).
  2. **Daemon:** `inbox_send` as system sender `watchdog`.
  3. **Classify the failure** with `classify_daemon_error()` into
     `daemon_down`, `config_bad` or `other`:
     - `ConnectionRefusedError` is `daemon_down`.
     - `FileNotFoundError` is `daemon_down` only when the path check passed and
       the error, if it names a file, names the daemon path itself. Otherwise
       it is `config_bad`.
     - `NotADirectoryError` is `config_bad`: a component of the path is a file.
     - Everything else is `other`: a timeout, an error reply from a running
       daemon, or `IsADirectoryError`.
  4. **Direct inbox row, only on `daemon_down`.** Nothing was sent, so nothing
     can duplicate. The dead-man writes one row straight into
     `~/.config/dunkingsheep/inbox.db` through `dunk_inbox.Inbox.send`. That
     uses the same columns the daemon reads, sender `watchdog`, and no nudge.
     It then reads the row back by id to confirm sender, mailbox and body. The
     daemon shows the row when it returns; if it never returns, a human finds
     it with `sqlite3`. There is one row per incident (a given stale
     heartbeat), and the cooldown starts only after a confirmed row.
  5. **Never around a slow or live daemon (`other`).** It makes no direct
     write and starts no cooldown; the next run retries.
  6. **Last resort.** If nothing was delivered, whatever the cause, it writes
     `watchdog_deadman_status.json`
     `{at, cause, error, heartbeat, target, daemon_path, attempted, config_error}`
     and a journal line. It exits 2 on `config_bad` and 1 otherwise, so the
     unit shows in `systemctl --user --failed`. The `cause` field is how a
     human tells a dead daemon from a broken config. No desk banner exists
     yet; surfacing the status file on the desk is a vp-of-agentry change.
- **Deliberate exclusion: `DaemonUnavailable` never triggers the direct
  write** (proposed by the implementer, accepted by the VP, i19851).
  `FlockClient.call` raises it only after `sendall`, when the daemon closes
  without replying. So the request has already reached a daemon that may have
  queued the alert before dying. A direct row there could duplicate the alert,
  and only a failed connect proves that nothing was sent. The cost is one
  missed cycle: the alert is retried on the next run, by which time a dead
  daemon is refusing connections (`daemon_down`). A future change must not
  widen the trigger to include it without a test that proves no duplicate.
- **Cadence** (VP i19870, i19872, i19874). The dead-man still never reads the
  watchdog config. Instead, each successful live watchdog run writes its
  effective cadence into the heartbeat it already touches, atomically:
  `{"schema": 1, "cadence_minutes": N, "at": T}`. Age is still the file's
  mtime, so a stale `at` cannot keep the dead-man quiet. The dead-man's limit
  is `multiplier x max(--cadence-minutes, recorded)`, with `recorded` clamped
  to 6 x `--cadence-minutes`. A mismatch between the two units can only make
  the dead-man more tolerant, never raise a false "dead" alarm, and never more
  than 6x late.
  - **Empty heartbeat** (the legacy touched file): no recorded cadence. The
    unit value is used silently.
  - **Non-empty but unparseable**, or no positive `cadence_minutes`: the unit
    value is used, plus one line naming the problem.
  - **Clamped:** one line naming the recorded value and the clamp.
  - **Where those lines go:** in a stale alert, they are part of the alert
    body. On a fresh heartbeat there is no alert, so they go to stderr (the
    journal) and to the `--json` report (`clamped`,
    `recorded_cadence_minutes`, `effective_cadence_minutes`,
    `heartbeat_problem`, `notes`). Never to the inbox. Exit 0.
  - **No owner path:** the dead-man has exactly one recipient, its `--to` VP
    mailbox, on both the daemon and the direct-write path. A test asserts this,
    so no heartbeat oddity can page the owner at any severity.
  - **Keep the units in step anyway:** the unit's `--cadence-minutes` is the
    floor and the basis of the clamp.
  - **Known residual (par-a, accepted by the VP as not a blocker):** the
    recorded cadence is the config's claim, not the timer's measured
    interval. A config saying 10 with a timer that fires every 60 minutes
    still raises a false alarm. Catching that would need the dead-man to
    compare successive mtimes.

## Sender identity

`inbox_send(system_sender="watchdog")` is accepted only for names in
`dunk_core.SYSTEM_SENDERS`, and only from a caller with no pane (a systemd
unit). This assumes every agent runs with `HERDR_PANE_ID` set.

## Install (by the VP, after Tier 1 review; not done by this change)

```sh
cp packaging/watchdog.example.json ~/.config/dunkingsheep/watchdog.json   # tune, then enabled=true, dry_run=false
cp packaging/systemd/dunkingsheep-watchdog{,-deadman}.{service,timer} ~/.config/systemd/user/
systemctl --user daemon-reload
systemctl --user enable --now dunkingsheep-watchdog-deadman.timer dunkingsheep-watchdog.timer
```

The daemon must be restarted to pick up `system_sender` before the first live
send.

Telegram cutover (t282 / desk PR74): bridge and consumer import the same
dunk_telegram_routing.resolve_telegram_channel function. It reads the nonsecret
telegram-routing.json beside telegram-status.json, with precedence
VP_TELEGRAM_CHANNEL > routing file channel > cos. An absent file takes the
same default path in both callers. The route must exist in the desk channel
registry (VP_CHANNELS_FILE, or channels.json / channels.example.json in the
desk root). Both services must share the routing directory and environment
overrides. Neither reader opens credential-only telegram.json.

Missing or different status channel is a self-error, not delivery. New pending
pages retain their desk channel; historical pages without it belong to VP and
cannot be confirmed by a CoS watermark. Owner posting remains unwired; this
change does not enable paging. A previous channel field in telegram.json must
be moved to the separate routing file before starting the revised bridge.

A pending-page channel mismatch is recorded once as terminal status undelivered, with timestamp and reason. It never sets confirmed_at, never produces a delivered recovery, and is not automatically re-posted; resolving that terminal record requires operator review. This prevents a cutover mismatch from emitting the same self-error every tick.

## Backlog starvation and current worker inventory

`fleet.assignment_starvation` detects a gap upstream of automatic handoff:
workers have no open/in-progress assignment while global open/blocked backlog
waits. Handoff deliberately selects **already assigned** work; it does not
allocate backlog or clear approval gates.

The rule is disabled when migrating an older configuration. Enable it explicitly
and set `worker_panes` to the actual worker inventory and `superuser_panes` to the
coordinators who should resolve allocation problems. Every alert destination must
also be present in the current `superusers.json` pane allowlist. Human panes and
canaries need not be included. The example placeholders must be replaced before
enabling the rule.

For tmux fleets use `panes.source: "daemon"`. This reads the running daemon's
canonical `list_panes` view with a five-second timeout, including declared turn
activity; the legacy `herdr` source remains available. An unavailable or malformed
inventory is reported as a coverage error, not an empty healthy fleet.

The deterministic predicate requires at least `min_empty_workers` configured,
present worker panes with no open/in-progress assignments, no live leases and
no native turn start/end or claim/done event in `quiet_minutes`. Private assigned
work excludes a worker too, but private backlog does not trigger a global alert.
Only idle/done states count as confirmed idle; unknown states are listed separately
as a visibility gap and must be checked before assigning work. Working/blocked
states and missing panes do not count. Old global open/blocked tasks supply the
backlog condition. "Open" does not assert runnable: dependencies and owner holds
remain intact. Other workers' review closures do not hide empty queues.

After `must_hold_runs` spaced observations, one fleet-level finding goes to the
configured superusers through the guarded inbox transport. It contains affected
workers, bounded task-ID samples, claim/done counts and a concrete request to
verify activity, rank work, assign runnable items or resolve gates. It does not
page the owner channel, enter keys, restart workers, reassign tasks or clear holds.
`cooldown_minutes` suppresses repeats. Successful recipient submissions are saved
individually, so retrying a failed recipient does not resend to successful ones.
A submitted inbox item is **not** a verified read or turn start. Once the condition
clears, its recipient state is discarded; a future episode must pass the hold
again. The existing VP recovery digest remains the recovery notification.

Task creation now defaults to `low` in the CLI, daemon API and store. Explicit
higher priorities remain unchanged. Existing task selection orders priority
first, then creation time (oldest first), then task ID for ties; updates do not
make a task younger. Historical priorities are not silently rewritten because
old records do not distinguish an explicitly selected medium priority from the
former default. Re-ranking such items is an explicit, recorded operator action.
