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

- **VP** is the dunkingsheep inbox. It is the **only** VP-only channel. Alerts
  arrive from sender `watchdog` (see below).
- **OWNER** is anything that reaches the owner's phone. Today that is a VP desk
  brief (`POST /api/brief`), which `telegram_bridge.send_briefs` forwards.
  **Every desk 'vp' brief reaches the owner**, so nothing meant for VP alone may
  go through the desk.
- **Delivery.** A page is DELIVERED only when the bridge's non-secret status
  file `vp-of-agentry/data/telegram-status.json`
  `{"last_brief_id": int, "updated_at": float}` shows `last_brief_id >= the
  posted brief id`. Until then the page is pending: it is never re-posted and no
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
| `task.lease_stale` | a lease is expired but not yet reaped, or was never renewed for at least N min, or a reap event (`expired`/`pane_gone`) falls in the lookback | `never_renewed_minutes`, `lookback_minutes` | warning / ops |
| `dunk.skip_loop` | the longest consecutive run of `dunk.skipped` in the lookback is at least N. Keyed by the run's first skip, so a growing loop is one incident | `consecutive_skips`, `lookback_hours` | warning / ops |
| `dunk.overdue` | a running dunk is at least N intervals past `next_send_at`, or has no scheduled send | `missed_intervals` | warning / ops |
| `inbox.undeliverable` | a mailbox's undeliverable backlog is at least N (no age limit) | `min_messages` | warning / ops |
| `inbox.unread_live` | a live pane's mailbox has at least N queued messages older than the age | `age_minutes`, `min_messages` | warning / ops |
| `inbox.unread_dead` | queued mail for a pane that no longer exists | `min_messages` | warning / ops |
| `inbox.ambiguous_sender` | a sender is a mailbox name that routes to a (live) pane, not a pane id. Samples are the first `sample_ids` by time (the sample ships 12) | `lookback_hours`, `sample_ids`, `require_live_pane` | info / ops |
| `lane.working_without_task` | a pane is working, holds no task, and has no task event in N min | `quiet_minutes` | info / ops |
| `lane.idle_holding_task` | a pane holds a task while idle, with no task, send or read activity in N min | `quiet_minutes`, `idle_statuses`, `holding_statuses` | warning / ops |
| `host.load` | load1 per CPU is at or above the threshold | `load1_per_cpu` | warning / ops |
| `host.tmp` | used fraction is at least 0.70 (warning) or at least 0.90 (critical), free bytes are at or below the floor, or the inode fraction is at or above the limit | `max_used_fraction`, `critical_used_fraction`, `min_free_bytes`, `max_inode_fraction`, `path` | warning / ops |
| `host.disk_full` | /tmp or / is at least 98% full | `max_used_fraction`, `paths` | **critical / outage, hold 2** |
| `host.heavy_slot_stale` | a heavy-job semaphore slot is older than N min (no holder pid exists) | `directory`, `prefix`, `max_age_minutes` | warning / ops |
| `security.secret_text` | a credential-shaped line appears in inbox, task or dunk text, re-verified on a second read; allowlisted line hashes are skipped | `patterns`, `allowlist_sha256`, `lookback_hours` | **critical / security** |
| `money.spend_projection` | never: there is no metering source. It cannot be enabled without `source` and `cap`, and always prints SKIPPED | `source`, `cap` | disabled / money |
| `health.failed` | `health.json` is stale, or reports problems | `stale_minutes` | critical / ops |
| `watchdog.self_error` | anything the watchdog could not do this run: a rule error, missing or corrupt state, or an owner post or confirmation problem. One finding per run | (none) | VP only |
| `watchdog.deadman` | the heartbeat is missing or older than 3x the cadence (separate process) | CLI flags | VP only |

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
  1. **Daemon first:** `inbox_send` as system sender `watchdog`.
  2. **Direct inbox row, only when the daemon is provably absent.** The trigger
     is `ConnectionRefusedError`, or `FileNotFoundError`/`NotADirectoryError` on
     the socket: nothing was sent, so nothing can duplicate. The dead-man then
     writes one row straight into `~/.config/dunkingsheep/inbox.db` through
     `dunk_inbox.Inbox.send` (the same columns the daemon reads; sender
     `watchdog`, no nudge), and reads it back by id to confirm sender, mailbox
     and body. The daemon shows the row when it returns; if it never returns, a
     human finds it with `sqlite3`. There is one row per incident (a given
     stale heartbeat), and the cooldown starts only after a confirmed row.
  3. **Never around a slow or live daemon.** A timeout, a reply-less close
     after the request was sent, or an error reply from a running daemon may
     mean the alert is already queued. So there is no direct write, and no
     cooldown; the next run retries.
  4. **Last resort.** If nothing could be delivered, it writes
     `watchdog_deadman_status.json` `{at, error, heartbeat, target, attempted}`
     and a journal line, and exits 1, so the unit shows in
     `systemctl --user --failed`. No desk banner exists yet; surfacing the
     status file on the desk is a vp-of-agentry change.
- **Cadence.** The cadence in the dead-man unit is hard-coded deliberately,
  because the dead-man must not read the watchdog config. Change both together.

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
