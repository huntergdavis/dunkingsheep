# Exception watchdog

**Review criterion:** the escalation behaviour below is reviewed against [WATCHDOG_ESCALATION_SPEC.md](WATCHDOG_ESCALATION_SPEC.md), written before this implementation. Where the code and the spec disagree, the code changes or the spec is argued in writing.

The watchdog replaces the VP's polling loop with deterministic alerts. It reads
the stores Dunking Sheep already keeps, applies fixed rules, and sends **one
deduplicated digest** to the VP inbox. It pages the owner only for a short list
of categories, and only once owner paging has been explicitly approved.

## Hard constraints (owner, VP i19781)

1. **Deterministic and configurable.** Every rule is a fixed predicate over
   read-only SQL and JSON reads. Every threshold, window, severity, recipient
   and cooldown lives in one config file. The same stores, config, state and
   `--now` always produce the same findings in the same order.
2. **No LLM at runtime, ever.** No model calls, no generated summaries, no
   "ask an agent". `tests/test_dunk_watchdog.py::NoModelTests` fails the build
   if `dunk_watchdog.py` or `dunk_watchdog_deadman.py` imports anything outside
   a short allowlist, shells out, or names a model vendor or tool. A test also
   proves that the guard catches such a call.
3. **Not a dashboard.** It alerts. Output is text or JSON on stdout, at most one
   inbox digest per recipient per run, and a derived `needs_attention` count.
   There is no UI, no HTTP server and no web page. Visualising state is the
   metrics collector's job.

## Inputs (read-only; no schema changes)

| Store | Used by |
| --- | --- |
| `tasks.db` (`tasks`, `task_leases`, `task_events`) | task and lane rules |
| `inbox.db` (`messages`) | inbox rules, pane-name routing history, pane activity |
| `metrics.db` (`events`, `dunk.sent` / `dunk.skipped` by dunk id) | skip-loop rule |
| `dunks.json`, `health.json` | dunk and health rules |
| host sample (`os.statvfs`, `os.getloadavg`) | host rules (replaces vp-of-agentry's `host_watch` warnings) |
| herdr pane snapshot (`herdr pane list`, local, read-only) | `inbox.unread_dead`, the two lane rules, and `require_live_pane` |

SQLite stores are opened with `mode=ro`. Message bodies are never read into
evidence; findings carry ids, counts and ages.

**Pane status is not in any store.** `agent_status` (idle, working, blocked or
unknown) exists only in herdr's live registry. That is why the lane rules use
the herdr snapshot. Set `panes.source` to `"none"` and those rules are reported
as `[skipped]`, never silently passed.

## Rules

Every rule has `enabled`, `severity` (`info`, `warning` or `critical`),
`category` and `cooldown_minutes`, plus the parameters below. The severity and
category values shown are from the shipped sample.

| Rule id | Fires when | Parameters | Sample |
| --- | --- | --- | --- |
| `task.blocked_stale` | a `blocked` task has had no update and no `task_event` for at least the threshold | `threshold_minutes` | warning / work |
| `task.lease_stale` | a lease has `expires_at <= now` (not yet reaped); or it was never renewed (`heartbeat_at <= claimed_at`) for at least N min; or a `requeued`/`released` event with reason `expired` or `pane_gone` falls inside the lookback, so a silent reap becomes an alert | `never_renewed_minutes`, `lookback_minutes` | warning / work |
| `dunk.skip_loop` | the longest run of consecutive `dunk.skipped` events for one dunk inside the lookback is at least N. This is a run inside a window, not "skipping right now", so a loop that ended a day ago is still reported | `consecutive_skips`, `lookback_hours` | warning / scheduling |
| `dunk.overdue` | a running dunk is at least N intervals past `next_send_at`, or is running with no scheduled send | `missed_intervals` | warning / scheduling |
| `inbox.undeliverable` | a mailbox has at least N messages in state `undeliverable`. This is the standing backlog, with no age limit | `min_messages` | warning / delivery |
| `inbox.unread_live` | a mailbox has at least N `queued` messages older than the age limit for a live pane (liveness reported as unknown if the pane source is off) | `age_minutes`, `min_messages` | warning / delivery |
| `inbox.unread_dead` | a mailbox has `queued` messages whose pane is not in the snapshot | `min_messages` | warning / delivery |
| `inbox.ambiguous_sender` | a message's sender is not a pane id but is a mailbox name that the inbox routes to a pane (optionally a live one). This is a historical audit, so the shipped severity is `info` | `lookback_hours`, `sample_ids`, `require_live_pane` | info / audit |
| `lane.working_without_task` | a pane is `working`, holds no `in_progress` task, and has no task event in N min | `quiet_minutes` | info / work |
| `lane.idle_holding_task` | a pane holds a task in a `holding_statuses` state (by lease, or by an assignee that resolves to it) while in an `idle_statuses` state, with no task, send or read activity in N min | `quiet_minutes`, `idle_statuses`, `holding_statuses` | warning / work |
| `host.load` | load1 per CPU is at or above the threshold | `load1_per_cpu` | warning / outage |
| `host.tmp` | used fraction, free bytes or inode fraction breaches its limit | `max_used_fraction`, `min_free_bytes`, `max_inode_fraction`, `path` | critical / outage |
| `health.failed` | `health.json` is older than N min, or reports `ok=false` or problems | `stale_minutes` | critical / outage |
| `watchdog.self_error` | one or more rules raised (for example, an unreadable store) | (none) | `self` / VP only |
| `watchdog.deadman` | the heartbeat is missing or older than `multiplier x cadence` (separate process) | CLI flags | critical / VP only |

## Config (`~/.config/dunkingsheep/watchdog.json`)

The schema is `dunkingsheep-watchdog/1`. **Every key is required and unknown
keys are refused.** A missing or invalid file stops the watchdog with
`refusing to start: config <dotted.key>: <reason>`, exit code 2, and sends
nothing. Nothing ever runs on a default nobody chose.
`packaging/watchdog.example.json` is the full sample. It ships quiet:
`enabled: false`, `dry_run: true`, owner paging off.

```text
schema                "dunkingsheep-watchdog/1"
enabled, dry_run      bool. Live alerts need enabled=true AND dry_run=false AND no --dry-run flag
cadence_minutes       > 0. Also the self-error cooldown, and the dead-man unit uses 3x it
max_alerts_per_run    int >= 1. Extra findings become one "N further finding(s) suppressed" line
state_path            dedupe/cooldown state, JSON, written atomically (tmp + fsync + rename)
heartbeat_path        touched after every successful LIVE run, never by a dry run
state_retention_days  forget dedupe keys not seen for this long
stores.config_dir     where tasks.db / inbox.db / metrics.db / dunks.json / health.json live
panes.source          "herdr" | "none"
panes.pane_id_pattern regex for a pane id (separates pane ids from mailbox names)
escalation.vp         {mailbox, min_severity}
escalation.owner      {enabled, mailbox, categories ⊆ [security, outage, money], min_severity,
                       approved_by}. When enabled, mailbox, approved_by and >= 1 category are required
rules.<id>            {enabled, severity, category, cooldown_minutes, ...rule parameters}
```

## Escalation, dedupe, cooldown, cap

- **VP first.** Findings at or above `escalation.vp.min_severity` go into one
  digest to the VP mailbox. `info` findings stay in the dry-run and JSON report
  unless the VP lowers the minimum.
- **Owner only on purpose.** An alert also goes to the owner only when owner
  paging is enabled with a named approver, its category is in the owner list,
  and its severity meets the owner minimum. The VP's rule is that this is
  switched on only after the stream has proven trustworthy. Self errors and
  dead-man alerts **never** reach the owner.
- **Dedupe key** is `rule|key`. A finding is re-sent only after its rule's
  `cooldown_minutes`. Cooldown state lives in the state file, so it survives a
  restart.
- **Cap.** At most `max_alerts_per_run` alerts per run, ordered by severity,
  then rule, then key. The overflow is one visible suppression line, never
  silence.

## The watchdog's own failure modes

- **Self errors.** A rule that raises is caught. All failures in one run become
  **one** `watchdog.self_error` finding with `source=watchdog-self`. Rule
  findings carry `source=watchdog-rule`, so the two are visibly distinct in
  text, JSON and the digest. The other rules still run. The self error goes to
  the VP only.
- **No self-paging loop.** If delivering the digest fails (for example, the
  daemon is down), the run prints `DELIVERY FAILED (source=watchdog-self)` to
  stdout and exits 1. It does **not** retry through the inbox, does not write
  state, and does not touch the heartbeat, so the dead-man check reports it
  instead.
- **Dead man (quis custodiet).** `dunk_watchdog_deadman.py` runs in its own
  systemd unit and timer. It does not import the watchdog, read its config or
  evaluate any rule. Its only input is the heartbeat file's mtime. If the file
  is missing or older than `multiplier x cadence`, it sends one
  `watchdog.deadman` alert to the VP, at most once per its own cooldown, which
  is tracked in its own state file. It therefore still alerts when the
  watchdog is disabled, misconfigured, crashing or never installed. A dry run
  never touches the heartbeat: a dry run is not coverage.
- **Sender identity.** Alerts arrive from sender `watchdog`, not from a fake
  agent. `inbox_send` gained a `system_sender` argument. It accepts only names
  in `dunk_core.SYSTEM_SENDERS` (`watchdog`), and only from a caller with **no
  pane**: a systemd unit. An agent always runs in a pane, so it cannot claim
  the name. That reasoning assumes every agent has `HERDR_PANE_ID` set.

## Commands

```sh
dunkingsheep watchdog --dry-run [--json] [--config PATH] [--now UNIX_TIME]
dunkingsheep watchdog                        # live, only if the config allows it
dunkingsheep watchdog-deadman --cadence-minutes 10 --multiplier 3 --cooldown-minutes 60 \
    --to "vp of agentry" [--heartbeat PATH] [--state PATH] [--dry-run] [--json]
```

Both are local commands. They read the stores directly and never start the
daemon. Only a live send talks to it.

## Install (after VP approval; not done by this change)

```sh
cp packaging/watchdog.example.json ~/.config/dunkingsheep/watchdog.json   # then tune
cp packaging/systemd/dunkingsheep-watchdog{,-deadman}.{service,timer} ~/.config/systemd/user/
systemctl --user daemon-reload
systemctl --user enable --now dunkingsheep-watchdog-deadman.timer dunkingsheep-watchdog.timer
```

The daemon must be restarted to pick up the `system_sender` argument before
the first live send.
