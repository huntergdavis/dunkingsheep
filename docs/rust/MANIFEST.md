# Port manifest (generated from main @ 038d9aa, 2026-10-29)

Everything the Rust port must reproduce, generated from the tree. Regenerate with `tools/gen_manifest.py` (TASKS.md T00) when main moves.

## Module map

| python | rust | crate | phase | notes |
|---|---|---|---|---|
| dunk_core.py (Flock, Dunker, Message) | flock/{mod,dunks,sends,commands_*}.rs | ds-core | 1 | split by method group; loops become tokio tasks (5.3) |
| dunk_api.py | api.rs + registry.rs | ds-core | 1 | handlers become typed arg structs; registry drives CLI + MCP |
| dunk_server.py | server.rs | ds-core | 1 | tokio UnixListener; SO_PEERCRED via nix; identity binding verbatim |
| dunk_client.py | client.rs | ds-core | 1 | identity resolution (TMUX_PANE vs tty) verbatim; service token |
| dunk_callers.py | proc.rs | ds-core | 1 | ROOT OF TRUST: port first, test exhaustively |
| dunk_roles.py | roles.rs | ds-core | 1 | ALLOW tables as const; notify-based refresh |
| dunk_superusers.py | roles.rs (legacy reader only) | ds-core | 1 | parse_allowlist for the completion late-check |
| dunk_tasks.py | tasks/{store,claim,metrics}.rs | ds-core | 1 | schemas verbatim; claim_next in the writer actor |
| dunk_next_job.py | tasks/next_job.rs | ds-core | 1 | |
| dunk_inbox.py | inbox/store.rs | ds-core | 1 | blob dir, digests, retention |
| dunk_terminal.py | terminal/{control,tmux,parse}.rs | ds-core | 1 | control mode + subprocess fallback (5.1) |
| dunk_tmux_guard.py, dunk_guard.py, dunk_guard_wrapper.py | ds-cli guard subcommands + the shim scripts (kept as sh) | ds-cli | 2 | the wrappers are shell; only the installer ports |
| dunk_panes.py | panes.rs | ds-core | 1 | lanes.json, retired-panes.json |
| dunk_activity.py | activity.rs | ds-core | 1 | turn state, TTL |
| dunk_stuck.py | stuck.rs | ds-core | 1 | pure state machine; port tests 1:1 |
| dunk_dispatch.py | dispatch.rs | ds-core | 2 | tick runs from the timer binary |
| dunk_owner.py | owner.rs | ds-core | 1 | owner-push.db |
| dunk_metrics.py | metrics.rs | ds-core | 1 | append-only events |
| dunk_state.py | state.rs | ds-core | 2 | agent-state.db capture |
| dunk_load.py, dunk_policy.py | load.rs, policy.rs | ds-core | 2 | /proc sampling, kill decisions |
| dunk_context.py | context/{store,ledger,digest}.rs | ds-core | 2 | markdown front matter parser; FTS index; ledger |
| dunk_memory.py | memory/{ingest,recall}.rs | ds-core | 2 | FTS5 bundled; JSONL ingest |
| dunk_github.py, dunk_review.py, dunk_poolrad.py | github.rs, review.rs, poolrad.rs | ds-core | 2 | jsonwebtoken RS256; reqwest |
| dunk_watchdog.py, dunk_watchdog_deadman.py | watchdog/{rules,deadman}.rs | ds-core | 2 | rule engine; same digests |
| dunk_health.py, dunk_usage.py, dunk_views.py, dunk_pm.py, dunk_help.py | health.rs, usage.rs, views.rs, pm.rs, help.rs | ds-core | 2 | views reads the desk db read-only until phase 3 |
| dunk_allocator.py, dunk_fallback.py, dunk_recovery*.py, dunk_native_session.py, dunk_http.py | allocator.rs, fallback.rs, recovery.rs, native_session.rs, http.rs | ds-core | 2 | http facade :8792 read-only; keep |
| dunk_mcp.py | ds-mcp | ds-mcp | 1 | stdio JSON-RPC; tools from the registry filtered by role |
| dunkingsheep (CLI) | ds-cli | ds-cli | 1 | generated parser + ported printers; local-only commands (context reads, github one-shots) |
| herdr_client.py | (dropped) | - | - | the herdr backend is retired (tmux default since 2026-10-24); keep the Terminal trait so a backend can return |
| dunking_sheep_tui.py, text_editor.py | ds-tui (ratatui) | ds-tui | 3 | |
| web/, bridges/, ui/ | ds-web (axum) + unchanged React | ds-web | 3 | FastAPI routes 1:1; the owner mailbox/report land here |


## CLI top-level commands

`allocation` `status` `turn` `poll` `list` `get` `add` `update` `remove` `start` `stop` `toggle` `stop-all` `fire` `send` `send-keys` `install` `guard` `inbox` `context` `load` `pm` `task` `gh-token` `git-credential` `review` `poolrad` `github` `whoami` `token` `superuser` `memory` `health` `watchdog` `watchdog-deadman` `usage` `recover` `doctor` `owner` `state` `roster` `message-state` `reply-stats` `project-summary` `api` `capability` `lane` `retired` `dispatch` `clear-box` `delivery` `flush` `messages` `cancel-message` `metrics` `panes` `workspaces` `new-workspace` `new-tab` `split` `start-agent` `respawn-pane` `rename` `focus` `close` `herdr` `herdr-help` `read` `shutdown` `serve` `tui` `mcp` `mcp-setup` `guide` `commands` `mcp-map`


## Files on disk (~/.config/dunkingsheep)

agent-state.db, context-ledger.db, desk.json, dialog-alerts.db, dispatch-state.json, dispatch.db, dispatch_lanes.json, dunks.json, github-app-author.json, github-app-reviewer.json, github-repos.json, github-tokens.json, health.json, inbox.db, lanes.json, launches.json, load-state.json, memory.db, metrics.db, owner-push.db, pm-routing.json, retired-panes.json, roles.json, service-tokens.json, superusers.json, tasks.db, tmux-panes.json, tmux-trial-panes.json, usage.json, watchdog.json, watchdog_deadman_state.json, watchdog_deadman_status.json, watchdog_state.json; plus `context/` (markdown store), `attachments/` (inbox blobs), `bin/` (wrappers), `system-token`, `web-token`, `github-app-*.pem`, `dunkingsheep.sock`, `daemon.log`.


## Systemd units (packaging/systemd)

dunkingsheep, dunkingsheep-deepcheck, dunkingsheep-dispatch, dunkingsheep-health, dunkingsheep-poolrad, dunkingsheep-review, dunkingsheep-state, dunkingsheep-watchdog, dunkingsheep-watchdog-deadman (each a .service, most with a .timer); dunkingsheep-web, dunkingsheep-telegram (FF-9).


## Commands (the registry) with role permissions

| command | required | params | mcp | owner-only | roles allowed |
|---|---|---|---|---|---|
| `add_dunk` | - | target, text, interval_minutes, name, start, only_when, max_sends, skip_if_unconsumed, hold_while_typing | y | y | owner |
| `allocation` | - | action, pane_id, pools, roles, skills, enabled, task_id, pool, revision, body, requires | n |  | owner, web |
| `cancel_message` | message_id | message_id | y |  | owner, web |
| `clear_box` | target | target, reason | y |  | owner |
| `close` | kind, target | kind, target, force | y | y | owner |
| `context_add` | body, cls, subject | body, cls, subject, type, authority, source, stated, recheck_after, supersedes, global_store, project_path | y |  | owner, system, web, worker, reviewer |
| `context_promote` | slug | slug, global_store, project_path | y | y | owner, web |
| `context_retire` | slug | slug, reason, global_store, project_path | y | y | owner, web |
| `context_supersede` | slug, body | slug, body, global_store, project_path | y |  | owner, web, worker, reviewer |
| `create_tab` | - | workspace, label, cwd, command, focus | y | y | owner |
| `create_workspace` | - | role, provider, model, label, cwd, command, focus, mailbox | y | y | owner |
| `delivery` | target | target, status | y |  | owner, system, web, desk, planner, worker, reviewer, anonymous |
| `delivery_capability` | - | target | y |  | owner, system, web, desk, planner, worker, reviewer, anonymous |
| `dispatch` | - | action, brief, task_id, lane, id, lanes, dry_run | y |  | owner, system, web, desk, planner, worker, reviewer, anonymous |
| `doctor` | - | - | y |  | owner, system, web, desk, planner, worker, reviewer, anonymous |
| `fire_dunk` | id | id, countdown_s, wait | y | y | owner |
| `flush_box` | target | target | y |  | owner, system, web, worker, reviewer |
| `focus` | kind, target | kind, target | y | y | owner |
| `get_dunk` | id | id | y |  | owner, system, web, desk, planner, worker, reviewer, anonymous |
| `get_message` | message_id | message_id | y |  | owner, system, web, desk, planner, worker, reviewer, anonymous |
| `gh_token` | - | kind | y |  | owner, system, worker, reviewer |
| `health` | - | repair, deep | y |  | owner, system, web, desk, planner, worker, reviewer, anonymous |
| `help` | - | markdown | y |  | owner, system, web, desk, planner, worker, reviewer, anonymous |
| `herdr` | command | command | y | y | owner |
| `herdr_help` | - | topic | y |  | owner, web |
| `inbox_ack` | message_ids | message_ids, mailbox | y |  | owner, web, desk, worker, reviewer |
| `inbox_archive` | mailbox | mailbox, before, state | y |  | owner, web, worker, reviewer |
| `inbox_cancel` | message_ids | message_ids | y |  | owner, web, worker, reviewer |
| `inbox_count` | - | mailbox, fresh | y |  | owner, system, web, desk, planner, worker, reviewer, anonymous |
| `inbox_delete` | message_ids | message_ids | y |  | owner, web, worker, reviewer |
| `inbox_fetch` | attachment_id | attachment_id, dest | y |  | owner, system, web, desk, worker, reviewer |
| `inbox_list` | - | mailbox, unread_only, limit | y |  | owner, web, worker, reviewer |
| `inbox_read` | - | mailbox, limit, peek | y |  | owner, system, web, desk, planner, worker, reviewer |
| `inbox_recover` | - | mailbox | y |  | owner, web, worker, reviewer |
| `inbox_send` | to, body | to, body, subject, attach, system_sender, notify | y |  | owner, system, web, worker, reviewer |
| `inbox_stats` | - | - | y |  | owner, system, web, desk, planner, worker, reviewer, anonymous |
| `inbox_status` | message_id | message_id | y |  | owner, system, web, desk, planner, worker, reviewer, anonymous |
| `lane_register` | role | pane, role, provider, model, label | y |  | owner, system, desk, planner, worker, reviewer, anonymous |
| `lane_set` | - | pane, mailbox | y | y | owner |
| `lane_unregister` | pane | pane | y | y | owner, web |
| `lanes` | - | action, pane, mailbox | y |  | owner, system, web, desk, planner, worker, reviewer, anonymous |
| `list_dunks` | - | fresh | y |  | owner, system, web, desk, planner, worker, reviewer, anonymous |
| `list_messages` | - | - | y |  | owner, system, web, desk, planner, worker, reviewer, anonymous |
| `list_panes` | - | - | y |  | owner, system, web, desk, planner, worker, reviewer, anonymous |
| `list_workspaces` | - | - | y |  | owner, system, web, desk, planner, worker, reviewer, anonymous |
| `memory_backfill` | - | action, limit | y | y | owner |
| `memory_context` | query | query, sessions | y |  | owner, system, web, desk, planner, worker, reviewer, anonymous |
| `memory_ingest` | path | path, harness, project | y | y | owner |
| `memory_recall` | query | query, limit, harness, project, since | y |  | owner, system, web, desk, planner, worker, reviewer, anonymous |
| `memory_search` | query | query, limit, harness, project, since | y |  | owner, system, web, desk, planner, worker, reviewer, anonymous |
| `memory_stats` | - | - | y |  | owner, system, web, desk, planner, worker, reviewer, anonymous |
| `memory_status` | - | - | y |  | owner, system, web, desk, planner, worker, reviewer, anonymous |
| `message_state` | message_ids | message_ids | y |  | owner, system, web, desk, planner, worker, reviewer, anonymous |
| `metrics` | - | window_s, since, until | y |  | owner, system, web, desk, planner, worker, reviewer, anonymous |
| `owner_poll` | - | pane_id, by | y |  | owner, system, web, desk |
| `owner_push` | target, body | target, body, subject | y |  | owner, system, web |
| `owner_receipts` | - | push_ids | y |  | owner, system, web, desk, planner, worker, reviewer, anonymous |
| `owner_stats` | - | - | y |  | owner, system, web, desk, planner, worker, reviewer, anonymous |
| `owner_take` | - | pane_id, by | y |  | owner, system, web, desk |
| `pm_digest` | project, payload | project, payload, to | y |  | owner, system, web, desk, planner, worker, reviewer, anonymous |
| `pm_routing` | - | - | y |  | owner, system, web, desk, planner, worker, reviewer, anonymous |
| `poll` | - | mailbox, limit, panes, tasks, task_scope, assignee, include_done, task_limit, check_herdr | n |  | owner, system, web, desk, planner, worker, reviewer, anonymous |
| `poolrad_progress` | - | - | y |  | owner, system, web, desk, planner, worker, reviewer, anonymous |
| `poolrad_sync` | - | limit, dry_run | y | y | owner, system, web |
| `project_summary` | - | project, path | y |  | owner, system, web, desk, planner, worker, reviewer, anonymous |
| `read_pane` | target | target, lines | y |  | owner, system, web, desk, worker, reviewer |
| `recover` | - | apply, record | y |  | owner, system, web |
| `remove_dunk` | id | id | y | y | owner |
| `rename` | kind, target, label | kind, target, label | y | y | owner |
| `reply_stats` | - | mailboxes | y |  | owner, system, web, desk, planner, worker, reviewer, anonymous |
| `respawn_pane` | pane, expected_pid, expected_start, argv, confirm_idle | pane, expected_pid, expected_start, argv, rollback_argv, confirm_idle, reason | y | y | owner, system |
| `retire_pane` | - | action, pane, reason | y |  | owner |
| `review_list` | - | - | y |  | owner, system, web, desk, planner, worker, reviewer, anonymous |
| `review_sync` | - | dry_run | y | y | owner, system, web |
| `roster` | - | - | y |  | owner, system, web, desk, planner, worker, reviewer, anonymous |
| `send_keys` | target, keys | target, keys, hold_while_typing, wait_s, desk_token | y |  | owner, system, web, worker, reviewer |
| `send_text` | target, text | target, text, hold_while_typing, wait_s, press_enter, desk_token | y |  | owner, web, worker, reviewer |
| `shutdown` | - | stop_all | n | y | owner |
| `split_pane` | target | target, direction, cwd, command, focus | y | y | owner |
| `start_agent` | name, command | role, provider, model, name, command, workspace, tab, split, cwd, focus | y | y | owner |
| `start_dunk` | id | id | y | y | owner |
| `state_capture` | - | source | y |  | owner, system, web, worker, reviewer |
| `state_latest` | - | pane_id, project, limit | y |  | owner, system, web, desk, planner, worker, reviewer, anonymous |
| `status` | - | check_herdr, full, fresh | y |  | owner, system, web, desk, planner, worker, reviewer, anonymous |
| `stop_all` | - | - | y | y | owner |
| `stop_dunk` | id | id | y | y | owner |
| `superuser_add` | entry | entry | y |  | owner |
| `superuser_recover` | - | name | y |  | owner, system, desk, planner, worker, reviewer, anonymous |
| `superuser_remove` | entry | entry | y |  | owner |
| `superuser_status` | - | - | y |  | owner, system, web, desk, planner, worker, reviewer, anonymous |
| `task_add` | title | title, scope, body, status, priority, assignees, parent_id, depends_on, multi_assignee, kind, author, repo, pr, head | y |  | owner, system, web, desk, planner, worker, reviewer |
| `task_assign` | id, assignees | id, assignees, mode | y |  | owner, system, web |
| `task_block` | id | id, reason | y |  | owner, system, web, worker, reviewer |
| `task_cancel` | id | id, reason | y |  | owner, system, web |
| `task_complete` | id | id, by, verdict | y |  | owner, web, worker, reviewer |
| `task_delete` | id | id | y |  | owner, web |
| `task_follow` | - | scope | y |  | owner, web, worker, reviewer |
| `task_followers` | - | - | y |  | owner, system, web, desk, planner, worker, reviewer, anonymous |
| `task_heartbeat` | id | id, lease_seconds | y |  | owner, web, worker, reviewer |
| `task_list` | - | scope, status, assignee, include_done, limit, kind, needs_owner | y |  | owner, system, web, desk, planner, worker, reviewer, anonymous |
| `task_metrics` | - | window_s, since, until, scope, bucket | y |  | owner, system, web, desk, planner, worker, reviewer, anonymous |
| `task_next` | - | scope, lease_seconds, allow_busy | y |  | owner, web, worker, reviewer |
| `task_note` | id | id, event, reason | y |  | owner, system, web, desk, planner, worker, reviewer |
| `task_read` | id | id | y |  | owner, system, web, desk, planner, worker, reviewer, anonymous |
| `task_reap` | - | - | n |  | owner, system, web |
| `task_release` | id | id | y |  | owner, web, worker, reviewer |
| `task_stats` | - | scope | y |  | owner, system, web, desk, planner, worker, reviewer, anonymous |
| `task_unblock` | id | id | y |  | owner, system, web, worker, reviewer |
| `task_unfollow` | - | - | y |  | owner, web, worker, reviewer |
| `task_update` | id | id, title, body, status, priority, assignees, depends_on, parent_id, block_reason, multi_assignee | y |  | owner, system, web, worker, reviewer |
| `toggle_dunk` | id | id | n | y | owner |
| `token_issue` | role | role, label | y | y | owner |
| `token_revoke` | label | label | y | y | owner |
| `turn_end` | - | pane, turn_id, reason | n |  | owner, system, web, desk, planner, worker, reviewer |
| `turn_start` | - | pane, ttl_s, source, handoff_id | n |  | owner, system, web, desk, planner, worker, reviewer |
| `update_dunk` | id | id, target, text, interval_minutes, name, only_when, max_sends, skip_if_unconsumed, hold_while_typing | y | y | owner |
| `whoami` | - | - | y |  | owner, system, web, desk, planner, worker, reviewer, anonymous |


## MCP tool groups

- **inbox**: send->inbox_send, count->inbox_count, list->inbox_list, read->inbox_read, recover->inbox_recover, status->inbox_status, ack->inbox_ack, cancel->inbox_cancel, delete->inbox_delete, stats->inbox_stats, fetch->inbox_fetch, archive->inbox_archive
- **task**: add->task_add, list->task_list, read->task_read, update->task_update, assign->task_assign, complete->task_complete, block->task_block, unblock->task_unblock, cancel->task_cancel, delete->task_delete, note->task_note, next->task_next, heartbeat->task_heartbeat, release->task_release, follow->task_follow, unfollow->task_unfollow, followers->task_followers, stats->task_stats, metrics->task_metrics, reap->task_reap
- **dunk**: list->list_dunks, get->get_dunk, add->add_dunk, update->update_dunk, remove->remove_dunk, start->start_dunk, stop->stop_dunk, stop_all->stop_all, fire->fire_dunk
- **views**: push->owner_push, take->owner_take, poll->owner_poll, stats->owner_stats, roster->roster, message_state->message_state, reply_stats->reply_stats, project_summary->project_summary, capture->state_capture, latest->state_latest
- **dispatch**: dispatch->dispatch, retire->retire_pane, unretire->retire_pane, retired->retire_pane, lanes->lanes, lane_set->lane_set, review_sync->review_sync, review_list->review_list, gh_token->gh_token, poolrad_sync->poolrad_sync, poolrad_progress->poolrad_progress, context_add->context_add, context_promote->context_promote, context_supersede->context_supersede, context_retire->context_retire
- **pane**: list->list_panes, read->read_pane, delivery->delivery, capability->delivery_capability, send->send_text, send_keys->send_keys, flush->flush_box, clear_box->clear_box, messages->list_messages, get_message->get_message, cancel_message->cancel_message
- **workspace**: list->list_workspaces, create->create_workspace, create_tab->create_tab, split->split_pane, start_agent->start_agent, respawn_pane->respawn_pane, rename->rename, focus->focus, close->close
- **superuser**: whoami->whoami, register->lane_register, unregister->lane_unregister, token->token_issue, revoke->token_revoke, status->superuser_status, add->superuser_add, remove->superuser_remove, recover->superuser_recover
- **memory**: recall->memory_recall, context->memory_context, search->memory_search, stats->memory_stats, status->memory_status, ingest->memory_ingest, backfill->memory_backfill
- **metrics**: summary->metrics, status->status, health->health
- **pm**: digest->pm_digest, routing->pm_routing
- **system**: help->help, herdr->herdr, herdr_help->herdr_help, recover->recover, doctor->doctor


## Error strings tests assert on (keep byte-identical)

- `/tmp/my-own-scratch`
- `01a18644`
- `CODEX_HOME`
- `DECLARED by more than one live pane`
- `DUNKINGSHEEP_TMUX_SOCKET`
- `Meta-dunking`
- `RAISED`
- `UNKNOWN`
- `[socket/cli only]`
- `a verdict needs evidence`
- `active turn`
- `add_dunk(`
- `alive_source|fixture`
- `already cancelled`
- `already done with verdict `
- `already exists`
- `already gone`
- `ambiguous`
- `attributed_cores`
- `before`
- `body or an attachment`
- `caller`
- `caller is not authenticated`
- `cannot be verified`
- `cannot change status`
- `canonical JSON`
- `changed during completion`
- `channel`
- `clamped to 60 min`
- `command is required`
- `completed by its reviewer`
- `confirm-idle`
- `confirmed by the turn`
- `confirmed idle`
- `could not create the workspace x: workspace limit`
- `could not rename tab w2:t2: nope`
- `could not start agent `
- `cwd mismatch`
- `daemon`
- `dead pane id is not a mailbox`
- `dead since 2026-10-24 08:00`
- `default`
- `did not submit`
- `direction`
- `dispatch`
- `does not support`
- `dunk_sqlite.py`
- `dunkingsheep-tests-`
- `duplicate task_started`
- `ever run`
- `exited immediately`
- `expected`
- `expected `
- `filed by the review timer`
- `gone at the end`
- `has no start`
- `herdr pane commands`
- `ignoring ambient DUNKINGSHEEP_CLAUDE_STATUS`
- `ignoring ambient DUNKINGSHEEP_DIR`
- `inbox.undeliverable`
- `is cancelled`
- `isolated per-lane`
- `keys are required`
- `kind must be`
- `label is required`
- `live task lease`
- `live wrapper|no codex child|rollback not verified`
- `malformed`
- `may not review`
- `may only act as the author App`
- `may only type into its own pane|requires`
- `missing required`
- `names are no longer matched`
- `names thread`
- `needs --verdict`
- `needs TMUX_PANE`
- `needs repo, pr and head`
- `never became a process`
- `never ran|No such file`
- `never ran|not found`
- `never ran|not found|cannot execute`
- `newest-body`
- `newline-delimited JSON`
- `no dunk with id`
- `no dunkingsheep daemon`
- `no lanes list`
- `no live pane`
- `no mailbox given`
- `no message with id`
- `no owner is registered`
- `no pane matches`
- `no process id`
- `no queued message`
- `no such file`
- `no tab matches`
- `no target`
- `no workspace matches`
- `non-guard tmux`
- `not a git repository`
- `not allowed`
- `not in the private state`
- `not inside pane`
- `not live`
- `not owned`
- `not permitted for role anonymous`
- `not permitted for role desk|requires a superuser`
- `not permitted for role web`
- `not permitted for role worker`
- `not permitted for role worker|requires a superuser`
- `not visible`
- `not yours`
- `not.*complet|owned|owner`
- `not.there`
- `nothing after the sigil`
- `nothing to submit`
- `only the owner supersedes a rule`
- `only the owner writes rules`
- `only to yourself`
- `outbox`
- `partial line`
- `pass --force`
- `private`
- `process start`
- `reason=dead_lane`
- `reason=unhoused_lane`
- `reason=unknown_name`
- `refused`
- `refused before mutation`
- `refusing to auto-start a tmux daemon`
- `refusing to close`
- `request`
- `requires a superuser`
- `requires a superuser|not permitted`
- `requires a superuser|not permitted for role`
- `requires the owner role`
- `requires the system token`
- `retry the request`
- `rolled back`
- `rollout does not exist`
- `scope must be`
- `selected real tmux`
- `self-pane`
- `send_text`
- `session_meta`
- `session_meta names`
- `shared`
- `slot.1`
- `split must be`
- `submission unconfirmed`
- `timed out`
- `type must be one of`
- `unattributed_cores`
- `unfinished turn.*turn-old`
- `unknown argument`
- `unknown command`
- `unknown provider`
- `unknown terminal backend`
- `unknown_name`
- `unread`
- `width too small`
- `working`


## Schemas (live databases, verbatim)

```sql
## tasks.db
CREATE TABLE tasks (
    id            TEXT PRIMARY KEY,
    scope         TEXT NOT NULL,
    title         TEXT NOT NULL,
    body          TEXT NOT NULL DEFAULT '',
    status        TEXT NOT NULL DEFAULT 'open',
    priority      TEXT NOT NULL DEFAULT 'medium',
    parent_id     TEXT,
    multi_assignee INTEGER NOT NULL DEFAULT 0,
    attempts      INTEGER NOT NULL DEFAULT 0,
    assignees     TEXT NOT NULL DEFAULT '[]',
    depends_on    TEXT NOT NULL DEFAULT '[]',
    created_at    REAL NOT NULL,
    updated_at    REAL NOT NULL,
    created_by    TEXT,
    completed_by  TEXT,
    completed_at  REAL,
    cancelled_at  REAL,
    block_reason  TEXT
, kind TEXT NOT NULL DEFAULT 'work', author TEXT, repo TEXT, pr INTEGER, head TEXT, verdict TEXT);
CREATE INDEX tasks_by_scope
    ON tasks (scope, status, created_at);
CREATE INDEX tasks_by_parent ON tasks (parent_id);
CREATE TABLE task_leases (
    task_id     TEXT NOT NULL,
    pane_id     TEXT NOT NULL,
    claimed_at  REAL NOT NULL,
    heartbeat_at REAL NOT NULL,
    expires_at  REAL NOT NULL, caller_pid INTEGER, caller_start TEXT, caller_name TEXT, assignee_added INTEGER NOT NULL DEFAULT 0,
    PRIMARY KEY (task_id, pane_id)
);
CREATE INDEX leases_by_pane ON task_leases (pane_id);
CREATE TABLE counters (name TEXT PRIMARY KEY, value INTEGER NOT NULL);
CREATE TABLE task_events (
    id          INTEGER PRIMARY KEY AUTOINCREMENT,
    task_id     TEXT NOT NULL,
    event       TEXT NOT NULL,
    at          REAL NOT NULL,
    actor       TEXT,
    scope       TEXT,
    priority    TEXT,
    status_from TEXT,
    status_to   TEXT,
    duration_ms INTEGER,
    meta        TEXT
);
CREATE TABLE sqlite_sequence(name,seq);
CREATE INDEX task_events_by_time ON task_events (at);
CREATE INDEX task_events_by_task ON task_events (task_id, at);
CREATE INDEX task_events_by_event ON task_events (event, at);
CREATE TABLE allocation_workers (
 pane_id TEXT PRIMARY KEY, pools TEXT NOT NULL, roles TEXT NOT NULL,
 skills TEXT NOT NULL, enabled INTEGER NOT NULL, registered_by TEXT NOT NULL,
 updated_at REAL NOT NULL, last_assigned REAL NOT NULL DEFAULT 0);
CREATE TABLE allocation_offers (
 task_id TEXT PRIMARY KEY, pool TEXT NOT NULL, role TEXT NOT NULL,
 requires TEXT NOT NULL, excluded TEXT NOT NULL, owners TEXT NOT NULL,
 offered_by TEXT NOT NULL, state TEXT NOT NULL DEFAULT 'ready');
CREATE TABLE allocation_reviews (
 source_id TEXT PRIMARY KEY, task_id TEXT NOT NULL UNIQUE,
 revision TEXT NOT NULL, author TEXT NOT NULL);
CREATE UNIQUE INDEX tasks_one_review_per_head ON tasks (kind, repo, pr, head) WHERE kind = 'review' AND head IS NOT NULL AND status IN ('open', 'in_progress', 'blocked');
## inbox.db
CREATE TABLE messages (
    id            TEXT PRIMARY KEY,
    mailbox       TEXT NOT NULL,
    mailbox_label TEXT,
    pane_id       TEXT,
    sender        TEXT,
    subject       TEXT,
    body          TEXT NOT NULL,
    state         TEXT NOT NULL,
    created_at    REAL NOT NULL,
    read_at       REAL,
    acked_at      REAL,
    cancelled_at  REAL,
    reason        TEXT,
    notify_state  TEXT,
    notify_reason TEXT
, archived_at REAL, digest_id TEXT, digest_updated_at REAL, notify_at REAL, notify_transport_id TEXT, notify_attempt INTEGER);
CREATE INDEX messages_by_mailbox
    ON messages (mailbox, state, created_at);
CREATE TABLE attachments (
    id          TEXT PRIMARY KEY,
    message_id  TEXT NOT NULL,
    filename    TEXT,
    media_type  TEXT,
    size        INTEGER NOT NULL,
    sha256      TEXT NOT NULL,
    created_at  REAL NOT NULL
);
CREATE INDEX attachments_by_message ON attachments (message_id);
CREATE TABLE counters (name TEXT PRIMARY KEY, value INTEGER NOT NULL);
CREATE INDEX messages_by_state
    ON messages (state, pane_id);
CREATE UNIQUE INDEX messages_by_digest ON messages (digest_id) WHERE digest_id IS NOT NULL;
CREATE TABLE deliveries (
    mailbox       TEXT PRIMARY KEY,
    message_ids   TEXT NOT NULL,
    delivered_at  REAL NOT NULL
);
## metrics.db
CREATE TABLE events (
    id          INTEGER PRIMARY KEY AUTOINCREMENT,
    at          REAL NOT NULL,
    kind        TEXT NOT NULL,
    pane_id     TEXT,
    mailbox     TEXT,
    sender      TEXT,
    source      TEXT,
    outcome     TEXT,
    reason      TEXT,
    duration_ms INTEGER,
    attempts    INTEGER,
    bytes       INTEGER
);
CREATE TABLE sqlite_sequence(name,seq);
CREATE INDEX events_by_time ON events (at);
CREATE INDEX events_by_kind ON events (kind, at);
## dispatch.db
CREATE TABLE standby (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    task_id TEXT,
    brief TEXT NOT NULL,
    lane TEXT,
    created_at REAL NOT NULL,
    taken_at REAL,
    taken_by TEXT,
    sent_message TEXT
);
CREATE TABLE sqlite_sequence(name,seq);
## agent-state.db
CREATE TABLE agent_state (
    at REAL NOT NULL,
    pane_id TEXT NOT NULL,
    project TEXT,
    harness TEXT,
    cwd TEXT,
    argv TEXT,
    session_id TEXT,
    model TEXT,
    model_reason TEXT,
    thinking TEXT,
    thinking_reason TEXT,
    state TEXT,
    state_source TEXT,
    state_reason TEXT,
    task_id TEXT,
    source TEXT
, session_reason TEXT);
CREATE INDEX agent_state_pane ON agent_state(pane_id, at DESC);
CREATE INDEX agent_state_time ON agent_state(at DESC);
## owner-push.db
CREATE TABLE owner_push (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    pane_id TEXT NOT NULL,
    target TEXT,
    subject TEXT,
    body TEXT NOT NULL,
    created_at REAL NOT NULL,
    taken_at REAL,
    taken_by TEXT
);
CREATE TABLE sqlite_sequence(name,seq);
CREATE INDEX owner_push_pane ON owner_push(pane_id, taken_at);
CREATE TABLE owner_poll (
    pane_id   TEXT NOT NULL,
    at        REAL NOT NULL,
    by        TEXT,
    PRIMARY KEY (pane_id)
);
## memory.db
CREATE TABLE schema_meta (
    key   TEXT PRIMARY KEY,
    value TEXT NOT NULL
);
CREATE TABLE sources (
    id          INTEGER PRIMARY KEY,
    harness     TEXT NOT NULL,
    path        TEXT NOT NULL UNIQUE,
    project     TEXT,
    mtime       REAL NOT NULL,
    size        INTEGER NOT NULL,
    sha256      TEXT,
    state       TEXT NOT NULL DEFAULT 'pending',
    attempts    INTEGER NOT NULL DEFAULT 0,
    next_at     REAL,
    lease_until REAL,
    last_ingest REAL,
    error       TEXT,
    metadata    TEXT
);
CREATE INDEX sources_by_state ON sources(state, next_at);
CREATE TABLE sessions (
    id          TEXT PRIMARY KEY,
    harness     TEXT NOT NULL,
    project     TEXT,
    cwd         TEXT,
    title       TEXT,
    started_at  REAL,
    updated_at  REAL,
    metadata    TEXT
);
CREATE INDEX sessions_by_project ON sessions(project, updated_at);
CREATE INDEX sessions_by_harness ON sessions(harness, updated_at);
CREATE TABLE chunks (
    id          INTEGER PRIMARY KEY,
    session_id  TEXT NOT NULL REFERENCES sessions(id) ON DELETE CASCADE,
    ordinal     INTEGER NOT NULL,
    role        TEXT,
    kind        TEXT,
    text        TEXT NOT NULL,
    token_count INTEGER,
    sha256      TEXT NOT NULL,
    created_at  REAL,
    metadata    TEXT
);
CREATE INDEX chunks_by_session ON chunks(session_id, ordinal);
CREATE UNIQUE INDEX chunks_dedupe
    ON chunks(sha256, session_id, ordinal);
CREATE VIRTUAL TABLE chunks_fts USING fts5(
    text,
    content='chunks',
    content_rowid='id',
    tokenize='porter unicode61',
    prefix='2 3'
);
CREATE TABLE IF NOT EXISTS 'chunks_fts_data'(id INTEGER PRIMARY KEY, block BLOB);
CREATE TABLE IF NOT EXISTS 'chunks_fts_idx'(segid, term, pgno, PRIMARY KEY(segid, term)) WITHOUT ROWID;
CREATE TABLE IF NOT EXISTS 'chunks_fts_docsize'(id INTEGER PRIMARY KEY, sz BLOB);
CREATE TABLE IF NOT EXISTS 'chunks_fts_config'(k PRIMARY KEY, v) WITHOUT ROWID;
CREATE TRIGGER chunks_ai AFTER INSERT ON chunks BEGIN
    INSERT INTO chunks_fts(rowid, text) VALUES (new.id, new.text);
END;
CREATE TRIGGER chunks_ad AFTER DELETE ON chunks BEGIN
    INSERT INTO chunks_fts(chunks_fts, rowid, text)
        VALUES ('delete', old.id, old.text);
END;
CREATE TRIGGER chunks_au AFTER UPDATE ON chunks BEGIN
    INSERT INTO chunks_fts(chunks_fts, rowid, text)
        VALUES ('delete', old.id, old.text);
    INSERT INTO chunks_fts(rowid, text) VALUES (new.id, new.text);
END;
CREATE TABLE ingest_queue (
    id          INTEGER PRIMARY KEY,
    source_id   INTEGER NOT NULL REFERENCES sources(id) ON DELETE CASCADE,
    state       TEXT NOT NULL DEFAULT 'queued',
    attempts    INTEGER NOT NULL DEFAULT 0,
    next_at     REAL,
    lease_until REAL,
    error       TEXT,
    created_at  REAL NOT NULL,
    finished_at REAL
);
CREATE INDEX ingest_queue_ready ON ingest_queue(state, next_at);
CREATE TABLE memory_events (
    id          INTEGER PRIMARY KEY,
    at          REAL NOT NULL,
    event       TEXT NOT NULL,
    actor       TEXT,
    latency_ms  INTEGER,
    hits        INTEGER,
    source      TEXT,
    metadata    TEXT
);
CREATE INDEX memory_events_by_time ON memory_events(at, event);
```


## Tests (132 files; port each 1:1 under rust/crates/*/tests or as unit tests in the module)

__pycache__ _bootstrap fakes fixtures test_backpressure_metric_reason test_busy_native_queue test_claude_turn_generation test_cli test_cli_guard test_cli_json_position test_cli_read_after_write test_c
li_respawn_dispatch test_codex_ambiguous_composer test_codex_turn_generation test_command_smoke test_composer_boundary test_deja_free test_dunk_activity test_dunk_agent_launch test_dunk_allocator test
_dunk_backend_default test_dunk_bootstrap_sweep test_dunk_callers test_dunk_cancelled_handoff test_dunk_carried_failure test_dunk_client_busy test_dunk_coalesce_freshness test_dunk_codex_busy_frame te
st_dunk_codex_composer test_dunk_codex_drafts_vs_echo test_dunk_codex_idle_footer test_dunk_complete_ownership test_dunk_complete_ownership_probe test_dunk_context test_dunk_context_governance test_du
nk_core test_dunk_delivery test_dunk_delivery_backoff test_dunk_dialog_alerts test_dunk_dispatch test_dunk_dispatch_allowlist test_dunk_fallback test_dunk_federated_recovery test_dunk_gate5_message te
st_dunk_github test_dunk_guard test_dunk_guarded_nudge_submits test_dunk_health test_dunk_heavy_slot test_dunk_http_resize test_dunk_http_terminals test_dunk_identity test_dunk_identity_tty test_dunk_
inbox test_dunk_integration_t514 test_dunk_lane_auto_declare test_dunk_lane_status test_dunk_list_panes_provenance test_dunk_load test_dunk_mcp test_dunk_memory test_dunk_metrics test_dunk_native_fram
e test_dunk_native_session test_dunk_native_typed_frame test_dunk_next_job_handoff test_dunk_next_job_plan test_dunk_next_job_wiring test_dunk_outbox test_dunk_outbox_restart test_dunk_owner_fds test_
dunk_owner_receipts test_dunk_pane_pid test_dunk_panes test_dunk_pm test_dunk_policy test_dunk_poll test_dunk_poll_floor test_dunk_poolrad test_dunk_process_group test_dunk_read_order test_dunk_receip
ts_regression test_dunk_recovery test_dunk_recovery_wiring test_dunk_require_server test_dunk_respawn_pane test_dunk_response_loss test_dunk_roles test_dunk_safe_queued_submit test_dunk_scheduled_reco
very test_dunk_server test_dunk_shell_bypass test_dunk_sqlite test_dunk_start_agent_verify test_dunk_state_capture_cost test_dunk_state_session test_dunk_stuck test_dunk_stuck_recovery test_dunk_style
d_state test_dunk_styled_wiring test_dunk_submit_honesty test_dunk_submit_when_box_unknown test_dunk_suite_hermetic test_dunk_superusers test_dunk_task_model test_dunk_task_pool test_dunk_tasks test_d
unk_tasks_threads test_dunk_test_leaks test_dunk_tmux_guard test_dunk_turn_confirmation test_dunk_turn_end_dispatch test_dunk_views test_dunk_watchdog test_dunk_watchdog_starvation test_federated_back
off test_herdr_client test_mcp_stdout_binary test_mechanism_census test_memory_parity test_model_command_suffix test_opencode_turn_generation.mjs test_opencode_turn_generation test_package_identity te
st_pi_poll_backoff test_pi_turn_generation test_recover test_retired_mailbox_archive test_t374_acceptance test_terminal test_text_editor test_tmux_guard_resolution test_turn_auth_diagnostics test_turn
_producer_installers test_usage test_verdict_evidence test_watchdog_ambiguous_queued 


## Inventory: every module, class, function, constant

## dunk_activity.py (121 lines)

> What is an agent DOING - from the tool that knows, not from the screen.  Screen scraping answered this for a year and lied in both directions: a working pi

- const `STATES` = `('working', 'waiting', 'idle', 'unknown')`
- const `IDLE_CLAIM_S` = `float(os.environ.get('DUNKINGSHEEP_IDLE_CLAIM_S', str(6 * 3600)))`
- `turn_state(state, now, idle_claim_s, harness)` — THE one rule for a declared turn. Returns (state_or_None, since, reason).
- `activity(pane, claude_status, turns, now)` — The best available answer about one lane's activity, with its source.
- `_claude(pane, claude_status, now)`
- `_pi(pane, turns, now)`
- `say(record)` — One line for a log or a table.
- `now_hours(at, now)`

## dunk_allocator.py (299 lines)

> Registered, deterministic allocation. Task DB is the ownership authority.  Registration and offers are explicit contracts, never inferred from task prose.

- const `SCHEMA` = `"\nCREATE TABLE IF NOT EXISTS allocation_workers (\n pane_id TEXT PRIMARY KEY, pools TEXT `
- `tags(values, required)`
- class **AllocationStore**
  - `__init__(tasks)`
  - `register(pane, pools, roles, skills, actor, enabled)`
  - `status()`
  - `_offer(task, pool, role, requires, excluded, actor)`
  - `offer(task_id, pool, requires, actor)`
  - `review(source_id, revision, body, pool, requires, author)` — One canonical review per source, immutable author exclusion.
  - `allocate(idle_callers)` — Fresh process-verified idle snapshot supplied by the guarded daemon.
- class **Allocator**
  - `__init__(flock)`
  - `tick()` — Bounded periodic watchdog pass, with native idle evidence, no AI.

## dunk_api.py (2799 lines)

> The Dunking Sheep command registry.  One table of named commands drives all three remote control surfaces:

- const `DEFAULT_SEND_WAIT_S` = `3.0`
- const `TARGET_DOC` = `"Which terminal pane. Accepts a pane id like '%N' (tmux) or 'w8:p3' (herdr rollback), the `
- const `TEXT_DOC` = `"Text to type into the pane, followed by Enter. Placeholders {id}, {name}, {count}, {targe`
- class **Command**
  - `__init__(name, description, handler, params, required, mcp, mcp_required)`
  - `input_schema()`
- `_opt(args, key, default)`
- `_bool(value, default)`
- const `TASK_ID_PARAM` = `{'type': 'string', 'description': 'Task id, e.g. t12.'}`
- const `TASK_SCOPE_PARAM` = `{'type': 'string', 'description': "'global' for the fleet queue, a pane id for a private q`
- const `TASK_SCOPE_ADD_PARAM` = `{'type': 'string', 'description': "'global' (default, fleet-wide), 'self' (private to the `
- const `TASK_SCOPE_NEXT_PARAM` = `{'type': 'string', 'description': "'global' (default), 'self' (own pane queue), 'auto' (pa`
- const `TASK_TITLE_PARAM` = `{'type': 'string', 'description': 'Short task title.'}`
- const `TASK_BODY_PARAM` = `{'type': 'string', 'description': 'Longer description or acceptance notes.'}`
- const `TASK_STATUS_PARAM` = `{'type': 'string', 'enum': list(TASK_STATUSES), 'description': 'open, in_progress, blocked`
- const `TASK_PRIORITY_PARAM` = `{'type': 'string', 'enum': list(TASK_PRIORITIES), 'description': 'urgent, high, medium or `
- const `TASK_ASSIGNEES_PARAM` = `{'type': 'array', 'items': {'type': 'string'}, 'description': "Pane ids or agent names. 's`
- const `TASK_PARENT_PARAM` = `{'type': 'string', 'description': 'Parent task id, for a subtask.'}`
- const `TASK_DEPS_PARAM` = `{'type': 'array', 'items': {'type': 'string'}, 'description': 'Task ids that must be done `
- const `TASK_LEASE_PARAM` = `{'type': 'number', 'minimum': 1, 'description': 'Lease length in seconds (default 1800).'}`
- const `SUPERUSER_ENTRY_PARAM` = `{'type': 'string', 'description': "A pane id (e.g. w9:p8) or a name matching a pane's labe`
- const `SUPERUSER_NAME_PARAM` = `{'type': 'string', 'description': "Name to re-assert (default 'VP of agentry')."}`
- const `MEMORY_QUERY_PARAM` = `{'type': 'string', 'description': 'Memory search text.'}`
- const `MEMORY_LIMIT_PARAM` = `{'type': 'integer', 'minimum': 1, 'maximum': 50, 'description': 'Maximum results (default `
- const `MEMORY_HARNESS_PARAM` = `{'type': 'string', 'enum': ['claude', 'codex', 'opencode', 'pi', 'generic'], 'description'`
- const `MEMORY_PROJECT_PARAM` = `{'type': 'string', 'description': 'Only this project label.'}`
- const `MEMORY_SINCE_PARAM` = `{'type': 'number', 'description': 'Only chunks updated at/after this unix timestamp.'}`
- const `MEMORY_PATH_PARAM` = `{'type': 'string', 'description': 'Session JSONL file or sqlite DB to ingest.'}`
- `h_status(flock, args, self_pane_id)`
- `h_turn_start(flock, args, self_pane_id)`
- `h_turn_end(flock, args, self_pane_id)`
- `h_list_dunks(flock, args, self_pane_id)`
- `h_poll(flock, args, self_pane_id)` — One batched read for sidebars: status + inbox + dunks.
- `h_get_dunk(flock, args, self_pane_id)`
- `h_add_dunk(flock, args, self_pane_id)`
- `h_update_dunk(flock, args, self_pane_id)`
- `h_remove_dunk(flock, args, self_pane_id)`
- `h_start_dunk(flock, args, self_pane_id)`
- `h_stop_dunk(flock, args, self_pane_id)`
- `h_toggle_dunk(flock, args, self_pane_id)`
- `h_stop_all(flock, args, self_pane_id)`
- `h_fire_dunk(flock, args, self_pane_id)`
- `_authorise_send(flock, command, args)` — Retired: the desk credential became the `web` service token (dunk_roles).
- `h_send_text(flock, args, self_pane_id)`
- `h_send_keys(flock, args, self_pane_id)`
- `h_flush_box(flock, args, self_pane_id)`
- `h_retire_pane(flock, args, self_pane_id)` — Retire a pane: stop feeding it, stop paging about it, keep its mail.
- `h_state_capture(flock, args, self_pane_id)`
- `h_state_latest(flock, args, self_pane_id)`
- `h_owner_push(flock, args, self_pane_id)` — Owner -> agent delivery with NO inbox row and NO typing.
- `h_owner_poll(flock, args, self_pane_id)` — The lane's harness says "I am polling" - including when there was nothing.
- `h_owner_take(flock, args, self_pane_id)` — What is waiting for this pane; the extension calls it about once a second.
- `h_owner_stats(flock, args, self_pane_id)`
- `h_owner_receipts(flock, args, self_pane_id)` — READ-ONLY: arrival timestamps for specific owner pushes (t66).
- `h_roster(flock, args, self_pane_id)`
- `h_message_state(flock, args, self_pane_id)`
- `h_reply_stats(flock, args, self_pane_id)`
- `h_project_summary(flock, args, self_pane_id)`
- const `DISPATCH_LANES_PATH` = `os.path.join(_CONFIG_DIR, 'dispatch_lanes.json')`
- `_dispatch_allowlist(flock, args, lanes)` — Which lanes the dispatcher may feed. EMPTY by default, and empty means nothing.
- `h_dispatch(flock, args, self_pane_id)` — The deterministic feeder: standby queue + a tick that fills idle lanes.
- `h_clear_box(flock, args, self_pane_id)` — Destroy an unsubmitted draft. Superuser only, and every use is logged.
- `h_delivery(flock, args, self_pane_id)`
- `h_list_messages(flock, args, self_pane_id)`
- `h_get_message(flock, args, self_pane_id)`
- `h_cancel_message(flock, args, self_pane_id)`
- `h_inbox_send(flock, args, self_pane_id)`
- `h_inbox_count(flock, args, self_pane_id)`
- `h_pm_digest(flock, args, self_pane_id)`
- `h_pm_routing(flock, args, self_pane_id)`
- `h_inbox_list(flock, args, self_pane_id)`
- `h_inbox_read(flock, args, self_pane_id)`
- `h_inbox_status(flock, args, self_pane_id)`
- `_ids(args)`
- `h_inbox_ack(flock, args, self_pane_id)`
- `h_inbox_recover(flock, args, self_pane_id)`
- `h_inbox_cancel(flock, args, self_pane_id)`
- `h_inbox_delete(flock, args, self_pane_id)`
- `_archive_before(value)`
- `h_inbox_archive(flock, args, self_pane_id)`
- `h_inbox_stats(flock, args, self_pane_id)`
- `h_inbox_fetch(flock, args, self_pane_id)`
- `_task_values(value)`
- `h_allocation(flock, args, self_pane_id)`
- `h_task_add(flock, args, self_pane_id)`
- `h_task_list(flock, args, self_pane_id)`
- `h_task_read(flock, args, self_pane_id)`
- `h_task_update(flock, args, self_pane_id)`
- `h_task_assign(flock, args, self_pane_id)`
- `h_task_complete(flock, args, self_pane_id)`
- `h_task_block(flock, args, self_pane_id)`
- `h_task_unblock(flock, args, self_pane_id)`
- `h_task_cancel(flock, args, self_pane_id)`
- `h_task_delete(flock, args, self_pane_id)`
- `h_task_note(flock, args, self_pane_id)`
- `h_task_next(flock, args, self_pane_id)`
- `h_task_heartbeat(flock, args, self_pane_id)`
- `h_task_release(flock, args, self_pane_id)`
- `h_task_follow(flock, args, self_pane_id)`
- `h_task_unfollow(flock, args, self_pane_id)`
- `h_task_followers(flock, args, self_pane_id)`
- `h_task_reap(flock, args, self_pane_id)`
- `h_task_stats(flock, args, self_pane_id)`
- `h_task_metrics(flock, args, self_pane_id)`
- `h_memory_recall(flock, args, self_pane_id)`
- `h_memory_context(flock, args, self_pane_id)`
- `h_memory_search(flock, args, self_pane_id)`
- `h_memory_stats(flock, args, self_pane_id)`
- `h_memory_status(flock, args, self_pane_id)`
- `h_memory_backfill(flock, args, self_pane_id)`
- `h_memory_ingest(flock, args, self_pane_id)`
- `h_health(flock, args, self_pane_id)`
- `h_recover(flock, args, self_pane_id)`
- `h_doctor(flock, args, self_pane_id)`
- `h_metrics(flock, args, self_pane_id)`
- `h_list_panes(flock, args, self_pane_id)`
- `h_read_pane(flock, args, self_pane_id)`
- `h_list_workspaces(flock, args, self_pane_id)`
- `h_delivery_capability(flock, args, self_pane_id)` — What the desk may do with this target: push queue or immediate send.
- `h_lanes(flock, args, self_pane_id)`
- `h_lane_set(flock, args, self_pane_id)`
- `h_create_workspace(flock, args, self_pane_id)`
- `h_create_tab(flock, args, self_pane_id)`
- `h_split_pane(flock, args, self_pane_id)`
- `h_start_agent(flock, args, self_pane_id)`
- `h_respawn_pane(flock, args, self_pane_id)`
- `h_rename(flock, args, self_pane_id)`
- `h_focus(flock, args, self_pane_id)`
- `h_close(flock, args, self_pane_id)`
- `h_herdr(flock, args, self_pane_id)`
- `h_herdr_help(flock, args, self_pane_id)`
- `h_help(flock, args, self_pane_id)`
- `h_superuser_status(flock, args, self_pane_id)`
- `h_superuser_add(flock, args, self_pane_id)`
- `h_superuser_remove(flock, args, self_pane_id)`
- `h_superuser_recover(flock, args, self_pane_id)`
- `h_whoami(flock, args, self_pane_id)`
- `h_context_add(flock, args, self_pane_id)`
- `h_context_promote(flock, args, self_pane_id)`
- `h_context_supersede(flock, args, self_pane_id)`
- `h_context_retire(flock, args, self_pane_id)`
- `h_gh_token(flock, args, self_pane_id)`
- `h_poolrad_sync(flock, args, self_pane_id)`
- `h_poolrad_progress(flock, args, self_pane_id)`
- `h_review(flock, args, self_pane_id)`
- `h_lane_register(flock, args, self_pane_id)`
- `h_lane_unregister(flock, args, self_pane_id)`
- `h_token_issue(flock, args, self_pane_id)`
- `h_token_revoke(flock, args, self_pane_id)`
- `h_shutdown(flock, args, self_pane_id)`
- const `ID_PARAM` = `{'type': 'string', 'description': "Dunk id, e.g. 'd3' (see list_dunks)."}`
- const `TARGET_PARAM` = `{'type': 'string', 'description': TARGET_DOC}`
- const `TEXT_PARAM` = `{'type': 'string', 'description': TEXT_DOC}`
- const `INTERVAL_PARAM` = `{'type': 'number', 'minimum': 0.01, 'description': 'Minutes between sends, greater than ze`
- const `NAME_PARAM` = `{'type': 'string', 'description': 'Optional human label for the dunk.'}`
- const `ONLY_WHEN_PARAM` = `{'type': 'string', 'enum': ['idle', 'any'], 'description': "'idle' holds each send until t`
- const `SKIP_IF_UNCONSUMED_PARAM` = `{'type': 'boolean', 'description': 'Backpressure: skip a scheduled send (no-op, send_count`
- const `HOLD_WHILE_TYPING_PARAM` = `{'type': 'boolean', 'description': "Never type over a human: before each send the daemon r`
- const `MAILBOX_PARAM` = `{'type': 'string', 'description': "Whose inbox: an agent name, a pane id, a unique label s`
- const `MESSAGE_ID_PARAM_INBOX` = `{'type': 'string', 'description': "Inbox message id, e.g. 'i42', as returned by inbox_send`
- const `MESSAGE_IDS_PARAM` = `{'type': 'string', 'description': 'One inbox message id, or several comma separated.'}`
- const `MESSAGE_ID_PARAM` = `{'type': 'string', 'description': "Message id, e.g. 'm3' (see list_messages)."}`
- const `LABEL_PARAM` = `{'type': 'string', 'description': 'Name to give it (how you will target it later).'}`
- const `CWD_PARAM` = `{'type': 'string', 'description': "Working directory for the new shell (default: the backe`
- const `RUN_COMMAND_PARAM` = `{'type': 'string', 'description': "Optional command line to type + Enter into the new pane`
- const `FOCUS_PARAM` = `{'type': 'boolean', 'description': "Switch the human's view to it (default false: create i`
- const `WORKSPACE_PARAM` = `{'type': 'string', 'description': "Workspace id ('w9'), label, unique label substring, or `
- const `TAB_PARAM` = `{'type': 'string', 'description': "Tab id ('w9:t2'), label, unique label substring, or 'se`
- const `KIND_PARAM` = `{'type': 'string', 'enum': ['workspace', 'tab', 'pane'], 'description': 'What `target` nam`
- const `KIND_TARGET_PARAM` = `{'type': 'string', 'description': "Id, label, unique label substring or 'self'. For panes `
- const `MAX_SENDS_PARAM` = `{'type': 'integer', 'minimum': 0, 'description': 'Remove the dunk automatically after this`
- const `COMMANDS` = `[Command('owner_push', "Deliver an OWNER message to an agent with no inbox row and no typi`
- const `COMMANDS_BY_NAME` = `{command.name: command for command in COMMANDS}`
- `mcp_commands()`
- const `MCP_GROUPS` = `(('inbox', 'Agent-to-agent mailbox. action=send (preferred over typing), count, list, read`
- class **GroupedTool**
  - `__init__(name, description, actions)`
  - `resolve(action)`
  - `input_schema()`
- `grouped_mcp_tools()`
- `mcp_tool_map()` — OLD flat command -> {tool, action, mcp}. Covers every registered command,
- const `MCP_TOOL_MAP` = `mcp_tool_map()`
- const `SUPERUSER_ONLY` = `{'add_dunk', 'update_dunk', 'remove_dunk', 'start_dunk', 'stop_dunk', 'toggle_dunk', 'stop`
- `dispatch(flock, name, args, self_pane_id)` — Run a named command. Raises DunkError for unknown commands or bad args.

## dunk_callers.py (247 lines)

> Who is asking? A process identity for task leases, independent of herdr.  The pane id (HERDR_PANE_ID / TMUX_PANE) is the registry key. The liveness check

- const `AGENT_NAMES` = `frozenset({'pi', 'claude', 'codex', 'gemini', 'cursor', 'devin', 'agy', 'cline', 'omp', 'm`
- const `MAX_DEPTH` = `12`
- const `DEAD_STATES` = `frozenset({'Z', 'X', 'x'})`
- `_stat(pid)` — Parse the fields we need from /proc/<pid>/stat, or None.
- `process_start(pid)`
- `process_alive(pid, start)` — True only for a non-zombie process whose starttime still matches.
- `_cmdline(pid)`
- `pane_agent_pids(pane_pid)` — Kernel process entries for the pane process itself plus its direct children.
- `agent_name(pid)` — The agent executable this process is, if any.
- `_caller(pid, start, name, pane_id, source)`
- `caller_from_peer(peer_pid, pane_id, max_depth)` — Resolve the stable caller behind a socket peer.
- `caller_alive(caller)` — Liveness for a stored caller dict; missing identity means not alive.
- `caller_belongs_to_pane(caller, pane_pid)` — Bind a kernel-attributed caller to the backend's pane process ancestry.
- `caller_pane_auth_failure(caller, pane_pid)` — Return the first failed binding condition, or None on authentication.

## dunk_claude.py (71 lines)

> Claude Code session status from the tool itself (owner #1173).  `claude agents --json` returns one object per live session with

- const `DEFAULT_TTL_S` = `float(os.environ.get('DUNKINGSHEEP_CLAUDE_STATUS_TTL_S', '20'))`
- class **ClaudeStatus**
  - `__init__(ttl, binary, timeout)`
  - `agents(fresh)` — The live session list, cached for `ttl`; None when unavailable.
  - `session_for_pane(pane, fresh)` — The one session whose cwd is the pane's cwd, or None.
  - `status_for_pane(pane, fresh)`

## dunk_client.py (559 lines)

> Client for the Dunking Sheep daemon socket, plus daemon auto-start.      client = FlockClient.connect_or_start()

- const `HERE` = `os.path.dirname(os.path.abspath(__file__))`
- const `CLI_PATH` = `os.path.join(HERE, 'dunkingsheep')`
- const `START_TIMEOUT_S` = `8.0`
- const `PROBE_RUNNING` = `'running'`
- const `PROBE_BUSY` = `'busy'`
- const `PROBE_ABSENT` = `'absent'`
- const `ABSENT_ERRNOS` = `{errno.ENOENT, errno.ECONNREFUSED, errno.ENOTSOCK, errno.EADDRNOTAVAIL}`
- const `BUSY_RETRY_DELAYS` = `(0.5, 2.0, 5.0)`
- const `SYSTEMD_UNIT` = `'dunkingsheep.service'`
- const `UNIT_PATH` = `os.path.expanduser('~/.config/systemd/user/dunkingsheep.service')`
- `_systemd_installed()` — True when the systemd user unit is installed.
- `_systemctl(*args)` — Run systemctl --user, supplying XDG_RUNTIME_DIR when the caller's shell
- class **DaemonUnavailable**(RuntimeError)
- class **DaemonBusy**(DaemonUnavailable)
- `_current_pane_id()` — The herdr pane this process is actually in, per herdr 0.9.5.
- class **IdentityRefused**(DunkError)
- `controlling_tty(pid, max_depth)` — The /dev/pts/N of the nearest process in this chain that has one.
- `pane_for_tty(tty, runner)` — (pane_id, reason): which tmux pane owns this terminal, or why we cannot say.
- `exported_pane_id(environ)` — The pane this process is actually in, per the terminal that owns it.
- `resolve_identity(explicit, environ, tty_lookup, detect, allow_unresolved)` — (pane_id, note) for this caller, refusing only a LYING environment.
- `_terminal_exists(lookup)` — True when the lookup's reason says a TERMINAL was found but did not resolve.
- `identity_refusal_for(cmd, self_pane_id, has_token, note)` — Why a pane-less caller may not run `cmd`, or None when it may (t602).
- class **FlockClient**
  - `__init__(socket_path, self_pane_id, timeout)`
  - `service_token()` — A service token for a pane-less process (web desk, timers), from
  - `call(cmd)` — Send one command; return its result or raise DunkError / OSError.
  - `__getattr__(name)`
  - `probe()` — Classify the daemon WITHOUT side effects: running / busy / absent.
  - `is_running()`
  - `connect_or_start(cls, socket_path, self_pane_id, start, timeout)` — Return a client whose daemon is reachable, starting one if needed.
  - `start_daemon()` — Start the daemon: hand off to systemd when it supervises, else spawn

## dunk_context.py (712 lines)

> The declarative context store: .dunkingsheep/ per project, one global.  Files are the source of truth (one Markdown entry with YAML-ish front matter).

- const `CLASSES` = `('ground-truth', 'fact', 'provisional', 'history')`
- const `AUTHORITIES` = `('owner', 'measured', 'agent')`
- const `TYPES` = `('principle', 'decision', 'reference', 'lesson', 'measurement', 'incident', 'rule', 'wiki'`
- const `OWNER_ONLY_TYPES` = `('rule', 'principle')`
- const `LEDGER_PATH` = `Path(os.environ.get('DUNKINGSHEEP_DIR', str(Path.home() / '.config' / 'dunkingsheep'))) / `
- const `SCOPES` = `('global', 'project')`
- const `DEFAULT_CAP` = `8192`
- const `PROVISIONAL_DAYS` = `7`
- const `GLOBAL_DIR` = `Path(os.environ.get('DUNKINGSHEEP_DIR', str(Path.home() / '.config' / 'dunkingsheep'))) / `
- class **ContextError**(ValueError)
- `_sha(text)`
- class **Ledger**
  - `__init__(path)`
  - `_connect()`
  - `close()`
  - `record(store_root_path, slug, text, cls, authority, by)`
  - `status(store_root_path, slug, text)` — 'recorded' | 'modified' | 'unrecorded' | 'bootstrap' (no rows at all).
  - `count()`
- `store_root(path, global_store)`
- `_slug(text)`
- `_today()`
- `_parse_front(text)`
- `_render(meta, body)`
- `_days(value)`
- `select_stores(project_path, which, cwd)` — The stores a command should report on, each labelled.
- `digest_both(project_path, which, max_bytes, subject, record)` — The session digest. Global ground truths are ALWAYS included.
- `_stale_mark(entry)` — ' (STALE ...)' when an entry is past its recheck_after; ground truths too.
- `report(project_path, which)` — stats/check/index over every selected store, always labelled.
- class **Store**
  - `__init__(path, global_store, ledger)`
  - `_record(path, slug, meta, by)`
  - `verified(entry, ledger)` — Does the file on disk match what the daemon wrote? (bootstrap counts.)
  - `adopt(ledger, by)` — Bootstrap: record every live entry as-is. Owner's one-shot.
  - `ensure(meta_lines)`
  - `exists()` — A store exists when it has a root with any class directory or README.
  - `entries()`
  - `find(subject_or_slug)`
  - `is_retired(entry)`
  - `live_entries()` — Current heads only; entries()/find() retain the historical audit trail.
  - `resolve(subject_or_slug)` — Resolve a subject or historical slug to its non-retired chain head.
  - `add(body, cls, subject, type_, authority, source, stated, recheck_after, supersedes, slug, by)`
  - `supersede(slug, new_body, by)`
  - `retire(slug, reason, by)`
  - `check()`
  - `index_path()`
  - `index_compatible(path)` — True only when the cached index is an FTS5 table that can answer the
  - `index()`
  - `search(query, cls, scope, limit)`
  - `access_path()`
  - `log_access(slug, by, kind)` — kind='read' when someone opened an article, 'injected' when it was delivered.
  - `reads()`
  - `stats()`
  - `entry_stats()` — Per-article access counts: what was delivered, what was opened, when.
  - `digest(max_bytes, subject)`

## dunk_core.py (9091 lines)

> Dunking Sheep core: the flock of dunkers and their timers.  This module is the single mechanism behind every control surface. The TUI's

- const `VERSION` = `'2.71.2'`
- `code_identity(path)` — Hash the flat application module tree, not just dunk_core.py.
- const `CODE_IDENTITY` = `code_identity()`
- `code_is_stale(identity, current)` — Compare the import-time application tree with the current disk tree.
- `code_status()` — One disk read per status request, including names and read failures.
- const `TASK_LEADER_ENV` = `'DUNKINGSHEEP_LEADER_PANE'`
- const `CONFIG_DIR` = `os.environ.get('DUNKINGSHEEP_DIR') or os.path.expanduser('~/.config/dunkingsheep')`
- const `STATE_FILE` = `os.path.join(CONFIG_DIR, 'dunks.json')`
- const `LAUNCHES_FILE` = `os.path.join(CONFIG_DIR, 'launches.json')`
- const `BUSY_STATUSES` = `('working', 'blocked')`
- const `_PANE_ID_RE` = `re.compile('^(?:[A-Za-z0-9_.-]+:p\\d+|%\\d+)$')`
- const `_HERDR_ID_RE` = `re.compile('^w\\d+:p[A-Za-z0-9_.-]+$')`
- const `PANE_ID_RE` = `_PANE_ID_RE`
- const `_TERM_ID_RE` = `re.compile('^term_[0-9A-Za-z]+$')`
- const `_EMBEDDED_PANE_RE` = `re.compile('\\(\\s*([A-Za-z0-9_.-]+:p\\d+|%\\d+)\\s*\\)\\s*$')`
- const `IDLE_POLL_S` = `5`
- const `MIN_IDLE_POLL_S` = `0.5`
- `idle_poll_wait(stop_event, idle_poll_s)` — One idle-poll wait, floored (t212).
- const `RESTORE_GRACE_S` = `5`
- const `DEFAULT_TEXT` = `'continue'`
- const `DEFAULT_INTERVAL_MINUTES` = `10.0`
- const `ONLY_WHEN_CHOICES` = `(None, 'idle')`
- const `TASK_REAP_S` = `float(os.environ.get('DUNKINGSHEEP_REAP_S', '15'))`
- const `TURN_TTL_S` = `float(os.environ.get('DUNKINGSHEEP_TURN_TTL_S', '1800'))`
- const `TURN_END_DISPATCH` = `os.environ.get('DUNKINGSHEEP_TURN_END_DISPATCH', '1').strip().lower() not in ('0', 'false'`
- const `TURN_END_WAIT_S` = `float(os.environ.get('DUNKINGSHEEP_TURN_END_WAIT_S', '4'))`
- const `MAILBOX_CACHE_S` = `float(os.environ.get('DUNKINGSHEEP_MAILBOX_CACHE_S', '30'))`
- const `VIEW_CACHE_S` = `float(os.environ.get('DUNKINGSHEEP_VIEW_CACHE_S', '5'))`
- const `TASK_PUSH_S` = `float(os.environ.get('DUNKINGSHEEP_PUSH_S', '10'))`
- const `CONSUMPTION_READ_LINES` = `40`
- const `CONSUMPTION_PREFIX_CHARS` = `80`
- const `INPUT_BOX_SIGILS` = `('❯', '›', '>')`
- const `MAX_UNKNOWN_SKIPS` = `3`
- const `COLLAPSED_INPUT_MARKERS` = `('ctrl+o to expand', 'paste again to expand', 'pasted text', 'pasted content', '[image')`
- const `TYPING_RECHECK_S` = `60`
- const `MESSAGE_RECHECK_S` = `5`
- const `TYPING_SETTLE_S` = `1.0`
- const `MESSAGE_HISTORY` = `50`
- const `DECIDED_KEPT` = `500`
- const `SUBMIT_GAPS` = `(0.2, 1.0, 3.0, 6.0)`
- const `GUARDED_PRESS_ATTEMPTS` = `3`
- const `GUARDED_VERIFY_GAPS` = `(0.2, 0.4)`
- const `SUBMIT_KEY_AFTER` = `2`
- const `REMEMBERED_SENDS` = `4`
- const `STALE_BOX_S` = `30.0`
- const `HOLD_BACKOFF_BASE_S` = `5`
- const `HOLD_BACKOFF_CAP_S` = `300`
- const `HOLD_ATTEMPT_LIMIT` = `10`
- const `MODAL_TAIL_LINES` = `8`
- const `MODAL_MARKERS` = `('esc to cancel', 'esc to close', 'esc to exit', 'enter to confirm', 'press enter to conti`
- const `NOTICE_PREFIX` = `'[inbox] '`
- const `DRAFT_CONFIRM_S` = `0.5`
- const `MODAL_WAIT_S` = `30.0`
- const `ENTER_KEYS` = `('Enter', 'enter', 'Return', 'return', 'KP_Enter')`
- const `SUBMIT_ATTEMPTS` = `len(SUBMIT_GAPS)`
- const `DEAD_SESSION_MARKERS` = `('server connection could not be restored', 'automatic reconnect could not restore this se`
- const `HARNESS_QUEUES_WHILE_BUSY` = `{'claude': True, 'pi': True, 'codex': True}`
- const `DELIVERY_TAIL_LINES` = `12`
- const `STATUS_TAIL_LINES` = `3`
- const `WORKING_MARKERS` = `('esc to interrupt', 'esc to cancel', 'ctrl+c to interrupt')`
- const `PAUSED_GOAL_MARKERS` = `('goal paused', 'paused (/goal resume)', '/goal resume')`
- const `CONTEXT_FULL_MARKERS` = `('/clear to save', 'context is full', 'context limit reached')`
- const `PLACEHOLDER_PATTERNS` = `(re.compile('new task\\?\\s*/clear to save [\\d.,]+[km]? tokens?', re.I), re.compile('^\\s`
- const `CLAUDE_BUSY_TTL_S` = `float(os.environ.get('DUNKINGSHEEP_CLAUDE_BUSY_TTL_S', '600'))`
- const `TYPING_LOCK_TIMEOUT_S` = `float(os.environ.get('DUNKINGSHEEP_TYPING_LOCK_TIMEOUT_S', '10'))`
- const `TYPING_LOCK_LOUD_S` = `float(os.environ.get('DUNKINGSHEEP_TYPING_LOCK_LOUD_S', '0.25'))`
- const `BOX_HOLD` = `'the box would not clear (text left untouched)'`
- const `SUBMIT_CONFIRM_S` = `2.0`
- const `SUBMIT_POLL_S` = `(0.1, 0.2, 0.4, 0.8, 1.0)`
- const `SUBMIT_VERIFY_GAPS` = `(0.4, 1.0)`
- const `TEMPLATE_RE` = `re.compile('\\{(id|name|count|target|interval|time|date)\\}')`
- const `ANSI_RE` = `re.compile('\\x1b\\[[0-9;?]*[ -/]*[@-~]')`
- const `DECORATION_RE` = `re.compile('[─-▟⠀-⣿\\s]+')`
- class **_NoticeFolded**(Exception)
- const `SYSTEM_SENDERS` = `('watchdog',)`
- class **DunkError**(ValueError)
- class **RetryableDunkError**(DunkError)
- class **AmbiguousTarget**(DunkError)
- `_as_dunk_error(method)` — An unknown message id is a caller mistake like any other, so it should
- `_truthy(value, default)`
- `format_interval(minutes)` — Human form of an interval: '10', '1.5', or '30s' for under a minute.
- `parse_interval(value)` — Accept minutes as a number or a string like '60', '1.5h', '90s', '2m'.
- `expand_template(text, dunker)` — Expand the known `{placeholders}` in `text`; leave other braces alone.
- class **Dunker**
  - `__init__(id)`
  - `target_label()`
  - `set_target(pane)` — Point at a terminal pane dict (as returned by the backend's list_panes_grouped).
  - `to_dict()`
  - `from_dict(cls, data)`
- class **Message**
  - `__init__(id, pane_id, target_label, text, source, deliver)`
  - `_queued_message()`
  - `outcome()`
  - `to_dict()`
- class **Flock**
  - `__init__(herdr, state_file, launches_file, persist, restore, inbox, metrics, tasks, superusers, memory, stuck_config, roles)`
  - `_stuck_config_source(explicit)` — Where the runtime stuck-recovery config comes from, in one documented order.
  - `_restore()`
  - `_save()`
  - `_load_launches()`
  - `_restore_outbox(rows)` — Rebuild held messages after a restart and log what came back.
  - `_save_launches()`
  - `_record_launch(pane_id)`
  - `_snapshot_launches(panes)` — Keep launches.json current from herdr's agent_session metadata.
  - `_find(dunk_id)`
  - `get(dunk_id)`
  - `list()`
  - `status(check_herdr)`
  - `poll_status()` — The cheap half of status: counts only, no global inbox/task scans.
  - `summary()`
  - `list_panes(self_pane_id)`
  - `lane_status(pane)`
  - `lane_activity(pane)` — What the lane is DOING, from the best evidence this backend has.
  - `_quarantine_shared_mailboxes()` — Move mail that an older resolver filed under an agent *type* out of
  - `pm_digest(project, payload, to, window, self_pane_id)` — Publish ONE digest per project under a stable id.
  - `resolve_mailbox(spec, self_pane_id, strict)` — Turn a target into (mailbox key, label, pane id).
  - `_is_agent_type(spec)` — True if `spec` names a kind of agent (claude, codex) rather than
  - `_mailbox_for_pane(pane)`
  - `view(self_pane_id, fresh)` — Cheap view for the poll-shaped commands: status + dunks + inbox count.
  - `_invalidate_view()`
  - `_own_mailbox(self_pane_id)` — The caller's mailbox key, briefly cached.
  - `lane_registry_path()`
  - `_lane_registry(strict)`
  - `_plain_lane_name(cls, name)` — True when this string may become a lane's identity BY INFERENCE.
  - `_usable_declared_name(cls, name)` — True when a DECLARED row holds something a lane could be called.
  - `_row_identity(value)` — (mailbox, declared) for a registry row, however it is spelled.
  - `delivery_capability(target, self_pane_id)` — How the desk may deliver to this target, and what it may show.
  - `_desk_send_rights(pane_id)` — Does the desk credential exist, and does it cover THIS pane?
  - `delivery_target(target, self_pane_id)` — Resolve a DIRECT-DELIVERY target (owner push, keystrokes) to one pane.
  - `lanes()` — The lane registry as rows: pane id, mailbox, declared?, alive?
  - `lane_set(pane_id, mailbox, self_pane_id)` — Declare who a pane IS, after the fact. Superuser-gated by the caller.
  - `_remember_lane(pane_id, mailbox, declared)` — Record pane id -> mailbox, so a backend flip cannot orphan a lane's mail.
  - `_write_lane_registry(data)` — Atomic replace of the lane registry. One writer, one rename.
  - `_title_pane(pane_id, mailbox)` — Best effort: put the lane name in the pane title too.
  - `_lane_pane(mailbox)` — (live pane, recorded mailbox) for a mailbox in the lane registry, else (None, None).
  - `_registered_lane(name)` — (found, alive, dead_since) for a mailbox name in the lane registry.
  - `_mailbox_exists(name)` — True when the inbox already holds mail addressed to this name.
  - `_mailbox_or_self(spec, self_pane_id, strict, own_only)`
  - `inbox_send(to, body, subject, attach, notify, self_pane_id, system_sender)` — Put a message in another agent's inbox. Nothing is typed into their
  - `inbox_archive(mailbox, before, states, self_pane_id)` — Bulk-retire a mailbox: mark messages read and archived, never delete.
  - `_notice_for(mailbox, unread, sender, subject)`
  - `_refresh_queued_notice(pane_id, text)` — If an inbox notice for this pane is still waiting, update it to the
  - `inbox_count(mailbox, self_pane_id)`
  - `inbox_list(mailbox, unread_only, limit, self_pane_id)`
  - `inbox_read(mailbox, limit, peek, self_pane_id)` — Read a batch WITHOUT consuming it, build the whole result, then commit.
  - `inbox_recover(mailbox, self_pane_id)` — Replay the latest delivered batch for display after a lost response.
  - `inbox_status(message_id)`
  - `inbox_ack(message_ids, mailbox, self_pane_id)` — Ack only messages in the caller's own mailbox. Sender is irrelevant;
  - `inbox_cancel(message_ids)`
  - `inbox_delete(message_ids)`
  - `inbox_stats()`
  - `inbox_fetch(attachment_id, dest)`
  - `_task_scope(scope, self_pane_id, default)` — Resolve a caller's scope and enforce per-pane privacy.
  - `_task_assignees(assignees, self_pane_id)` — Expand 'self'/'me' to the caller's pane id.
  - `_default_task_owner(self_pane_id)` — Who owns a task created WITHOUT `--assign` (owner #1708, wiring b).
  - `_require_task_visible(task, self_pane_id)`
  - `task_add(title, scope, body, status, priority, assignees, parent_id, depends_on, multi_assignee, self_pane_id, kind, author, repo, pr, head)`
  - `task_list(scope, status, assignee, include_done, limit, self_pane_id, kind, needs_owner)`
  - `task_read(task_id, self_pane_id)`
  - `task_update(task_id, title, body, status, priority, parent_id, assignees, depends_on, block_reason, multi_assignee, self_pane_id, updated_by)`
  - `task_assign(task_id, assignees, mode, self_pane_id)`
  - `task_complete(task_id, by, self_pane_id, verdict)`
  - `_require_completion_binding(self_pane_id)` — Refuse a pane claim the kernel evidence does not support (t423 P1).
  - `_authorize_task_complete(task, self_pane_id, operator)` — Refuse foreign completion BEFORE any mutation, event or handoff (t423).
  - `task_block(task_id, reason, self_pane_id)`
  - `task_unblock(task_id, self_pane_id)`
  - `task_cancel(task_id, reason, self_pane_id)`
  - `task_delete(task_id, self_pane_id)`
  - `task_note(task_id, event, reason, self_pane_id)` — Append an explanatory event (e.g. why a task was removed).
  - `require_turn_caller(pane_id, self_pane_id)` — Authenticate socket turn events; request pane labels alone prove nothing.
  - `turn_start(pane_id, ttl_s, source, self_pane_id, handoff_id)` — Declare a busy turn for `pane_id` (pi/claude hook or extension).
  - `turn_end(pane_id, reason, self_pane_id, turn_id, authenticated)`
  - `_dispatch_on_turn_end(pane_id)`
  - `_dispatch_claimed_turn_end(pane_id)` — Prompt a lane that ended its turn still holding work. Returns its record.
  - `turn_active(pane_id)` — True only for an unexpired declared turn. Expiry is the safety.
  - `turn_expire(now)` — Release expired turns: record the reason, reconcile the caller pid.
  - `set_caller(caller)`
  - `current_caller()`
  - `clear_caller()`
  - `set_socket_peer(ok)` — Record whether this request arrived over the socket with verifiable
  - `clear_socket_peer()`
  - `_may_pull_everything(pane_id)` — True when this pane's role may pull any kind (the owner).
  - `_claim_filters(self_pane_id)` — (kinds, exclude_author) for a claim, from the caller's role.
  - `task_next(scope, lease_seconds, self_pane_id, allow_busy)` — Atomically claim the next task; subscribe if the queue is empty.
  - `task_follow(scope, self_pane_id)` — Ask the daemon to push the next task when this pane is idle.
  - `task_unfollow(self_pane_id)`
  - `task_followers()`
  - `task_heartbeat(task_id, lease_seconds, self_pane_id)`
  - `task_release(task_id, self_pane_id)`
  - `task_reap(self_pane_id)`
  - `task_stats(scope, self_pane_id)`
  - `task_metrics(window_s, since, until, scope, bucket, self_pane_id)` — Deterministic, timeseries-friendly task metrics for the CTO.
  - `_memory_required()`
  - `memory_recall(query, limit, harness, project, since)`
  - `memory_context(query, sessions)`
  - `memory_search(query, limit, harness, project, since)`
  - `memory_stats()`
  - `memory_status()`
  - `memory_backfill(action, limit)` — Discover every known session source and enqueue it (throttled,
  - `memory_ingest(path, harness, project)`
  - `health(repair, deep)` — Deterministic self-check; see dunk_health.run_health.
  - `_start_task_workers()`
  - `_clear_historical_errors(panes)` — Clear a stale 'target pane not found' once the pane is back.
  - `_archive_retired_inbox(pane_list)` — Revalidate retirement and bindings at mutation; uncertain identity stays queued.
  - `_task_reaper_loop()`
  - `_task_push_loop()`
  - `_push_tasks()` — Feed idle followers one task each, using the same idle signal as
  - `_announce_task(pane_id, task, lease_seconds)`
  - `metrics_summary(window_s, since, until)` — A read-only snapshot of how delivery is going, for a status board.
  - `list_workspaces()` — Workspaces with their tabs nested, sorted by number.
  - `_self_pane(self_pane_id)`
  - `_pick(items, spec, kind, id_key, label_key)` — Resolve `spec` against a list: exact id, exact label (case-
  - `resolve_workspace(spec, self_pane_id)` — Workspace dict from an id ('w9'), a label, a unique label substring
  - `resolve_tab(spec, self_pane_id)` — Tab dict from an id ('w9:t2'), a label, a unique label substring or
  - `_pane_info(pane_id)`
  - `_role_pane_info(pane_id)` — Pane record for role binding, cached 2 s: one tmux listing per
  - `caller_role(self_pane_id)` — The caller's role for this request: token first, then the registry.
  - `set_service_token(token)`
  - `is_superuser(self_pane_id)`
  - `require_superuser(self_pane_id, action)` — Fail closed: only the owner role may do destructive things.
  - `whoami(self_pane_id)`
  - `lane_register(pane, role, provider, model, label, self_pane_id)` — Bind a role to a pane's root process. Owner/system only, except the
  - `lane_unregister(pane, self_pane_id)`
  - `token_issue(role, label, self_pane_id)` — Mint a service token for a pane-less process (web desk, timers).
  - `token_revoke(label, self_pane_id)`
  - `_github_tokens()`
  - `gh_token(kind, self_pane_id)` — A short-lived installation token for the App this caller's role implies.
  - `review_sync(dry_run, self_pane_id)`
  - `poolrad_sync(limit, dry_run, self_pane_id)`
  - `poolrad_progress(self_pane_id)`
  - `review_list(self_pane_id)`
  - `_require_review_evidence(task, self_pane_id)` — Refuse a verdict unless this pane noted its evidence after it claimed.
  - `_review_evidence(task)` — The reviewer's own note text, verbatim, for the follow-up and GitHub.
  - `_after_review_verdict(task, verdict, by)`
  - `_context_store(global_store, project_path)`
  - `context_add(body, cls, subject, type_, authority, source, stated, recheck_after, supersedes, global_store, project_path, self_pane_id)` — Write an entry through the daemon. Rules come only from the owner:
  - `context_promote(slug, global_store, project_path, self_pane_id)`
  - `context_supersede(slug, body, global_store, project_path, self_pane_id)`
  - `context_retire(slug, reason, global_store, project_path, self_pane_id)`
  - `superuser_status(self_pane_id)`
  - `superuser_add(entry, self_pane_id)` — Legacy spelling of `lane register --role owner <pane>`; names refused.
  - `superuser_remove(entry, self_pane_id)`
  - `superuser_recover(self_pane_id, name)` — Bootstrap: register the caller's pane as owner when none exists.
  - `require_send_self(target, self_pane_id, action)` — Non-superusers may only type into their own pane.
  - `_resolve_kind(kind, target, self_pane_id)`
  - `_after_create(ok, result, command, what)`
  - `create_workspace(label, cwd, command, focus, mailbox, role, provider, model, self_pane_id)` — New workspace (with its first tab and pane). `command`, if given, is
  - `create_tab(workspace, label, cwd, command, focus, self_pane_id)` — New tab. `workspace` is an id, label or 'self'; omitted means the
  - `split_pane(target, direction, cwd, command, focus, self_pane_id)` — Split an existing pane (any pane target) right or down.
  - `_sandbox_workspace_id()` — The dedicated workspace where spawned agents go by default.
  - `start_agent(name, command, workspace, tab, split, cwd, focus, self_pane_id, role, provider, model)` — Launch `command` in a new pane registered by name as agent `name`
  - `respawn_pane(pane_id, argv, rollback_argv, expected_pid, expected_start, confirm_idle, reason, self_pane_id)` — Replace a managed pane's process in place, session-preserving.
  - `_agent_evidence(root_pid, names, since_start)` — Descendant pids that are a RUNNING process of an expected agent.
  - `_verify_agent_started(name, pane_id, kind)` — Refuse to report 'started' for a pane whose command never ran.
  - `_match_agent_process(entries, kind, since_start)` — Pids with kernel evidence of running `kind`.
  - `_is_empty_shell(pane)`
  - `_resume_command(agent, session)`
  - `_launch_for(pane_id, dunker, entry, cwd)`
  - `_with_cwd(cwd, command)`
  - `_doctor_warnings(by_id)`
  - `_guard_status()`
  - `recover(apply, record, self_pane_id)` — Plan (and optionally apply) post-upgrade agent recovery.
  - `doctor(self_pane_id)` — Read-only post-upgrade check of the herdr surfaces and dunk metadata.
  - `rename(kind, target, label, self_pane_id)`
  - `focus(kind, target, self_pane_id)`
  - `close_target(kind, target, self_pane_id, force)` — Close a workspace, tab or pane, killing whatever runs inside.
  - `herdr_command(command, self_pane_id)` — Run any `herdr <subcommand ...>` and return its parsed result. This
  - `herdr_help(topic)` — herdr's own usage text: the command groups (`topic` empty) or one
  - `resolve_target(spec, self_pane_id)` — Turn a target spec into a pane dict.
  - `add(target, text, interval_minutes, name, start, only_when, max_sends, skip_if_unconsumed, hold_while_typing, self_pane_id)`
  - `update(dunk_id, target, text, interval_minutes, name, only_when, max_sends, skip_if_unconsumed, hold_while_typing, self_pane_id)` — Change fields on a dunk. Pass `only_when=None` / `max_sends=None`
  - `remove(dunk_id)`
  - `start(dunk_id)`
  - `stop(dunk_id)`
  - `toggle(dunk_id)`
  - `stop_all()`
  - `_start(dunker)`
  - `_spawn_timer(dunker)`
  - `_halt(dunker)` — Stop the timer thread without touching the persisted running flag.
  - `fire(dunk_id, countdown_s, wait)` — Send a dunk's text right now (the classic 't' test send). Does not
  - `_fire_worker(dunker, countdown_s)`
  - `send_now(target, text, self_pane_id, hold_while_typing, wait_s, press_enter, is_notice, notify_for, source)` — One-off send to any pane, no dunk involved. Like a dunk, it never
  - `send_keys_now(target, keys, self_pane_id, hold_while_typing, wait_s)` — Press keys in a pane, queued behind a human who is typing exactly
  - `_new_message(pane_id, label, text, source, deliver)`
  - `_deliver(message, now)` — Send one message, re-checking the box under the send lock. Returns
  - `_record_delivery_outcome(message, ok, detail, outcome, metric)` — The ONE end-of-life path for a decided message (sent, failed or undeliverable).
  - `_note_stuck_candidate(message)` — Keep the pane's LATEST delivery intent for the stuck observer (t377).
  - `_attempt_context(message)`
  - `_persist_typed_attempt(pane_id, text, press_enter)` — Save the actual typing receipt before Enter, including across a crash.
  - `_owned_recovery_evidence(pane_id, screen)` — t508: extra authority for an otherwise undelimitable Codex frame.
  - `_reconcile_stuck(pane_id, message_id, outcome, detail, created_at)` — Keep the original failure visible alongside its later recovery receipt.
  - `_publish_stuck_receipt(pane_id, entry)` — Idempotent receipt repair, retried after restart without another key.
  - `_remember(message)`
  - `_enqueue(message)` — Queue a message for its pane and make sure a delivery thread runs.
  - `_unwedge(pane_id, content, since, message, now, expect_message_id, require_intent)` — t377: recovery for text WE typed that is still sitting in the composer.
  - `_press_stuck_enter(pane_id, expected_fingerprint, generation, message_id, tracked_text, message, require_intent, expected_created_at)` — Exactly ONE Enter, under the shared locks, re-validated immediately before.
  - `_stranded_message(pane_id, content)` — Which message is actually stuck in this box?
  - `_stuck_candidate_message(pane_id)` — Reconstruct a bound failed delivery; never adopt the current pane.
  - `_sent_schedule_pending(pane_id, entry, screen)` — A reported SENT schedule needs extra proof before any recovery.
  - `_scheduled_recovery_current(entry)` — Do not replay expired reminders or instructions replaced by an edit.
  - `_text_identity(content)` — A stable identity for text with no Message row (a dunk's own notice).
  - `_stuck_text_is_ours(pane_id, tracked_text, content)` — EXACT ownership for the recovery path - never the prefix matcher.
  - `_stuck_status(pane_id)` — The delivery state for the recovery decision: deliverable is not enough,
  - `_pane_generation(pane_id)` — Pane identity that changes when a pane is replaced: pid AND process start.
  - `_federated_composer(screen)` — Authorized live input under the explicit all-input-via-DS policy.
  - `_federated_recovery_step(now)` — Deterministic preserve-submit, using the existing durable attempt ledger.
  - `_dialog_observer_step(now)` — Scan live agent dialogs every 30s even without queued mail. No keys.
  - `_stuck_observer_step(now)` — ONE periodic scan for a stalled delivery nobody else is watching (t377).
  - `_stuck_observer_loop()`
  - `_outbox_worker(pane_id)` — Deliver a pane's queued messages in order, one deterministic step at a time.
  - `_outbox_step(pane_id, now)` — ONE iteration of the outbox worker. Returns empty|delivered|held|due|stopped.
  - `list_messages(why_blocked)` — Queued messages (oldest first) and the recent delivered/failed ones.
  - `get_message(message_id)`
  - `cancel_message(message_id)` — Drop a queued message before it is delivered.
  - `read_pane(target, lines, self_pane_id)`
  - `_do_send(dunker)`
  - `_previous_send_consumed(dunker)` — Decide, at send time, whether the previous send has left the target's
  - `read_screen(pane_id, lines, ansi)` — The one way a component asks the pane what it looks like.
  - `read_box(pane_id)` — (screen, text, detail) for a pane's input box, from one read.
  - `_typing_in(pane_id, last_sent_text, awaiting_consumption)` — True if someone is composing text in `pane_id`'s input box. See
  - `send_blocker(pane_id, last_sent_text, awaiting_consumption)` — Why a send cannot go now, as (state, reason), or None if it can.
  - `_pane_harness(pane_id)`
  - `_clear_to_send(pane_id, last_sent_text, awaiting_consumption, stop_event)` — True once the box has read clear on two checks TYPING_SETTLE_S
  - `_box_holds(pane_id, text)` — True if `text` still sits in the pane's input box, False if nothing
  - `_session_process_gone(pane_id)` — True when the lane's own recorded process is no longer running.
  - `_dead_session(pane_id)` — Return the dead-session marker showing at the BOTTOM of the pane, or None.
  - `_send_submit_key(pane_id, fallback, busy_hint)` — Submit using the harness's current queue action, under the caller's lock.
  - `_flush_once(pane_id, text)` — One Enter, one short check. Used INSIDE the typing lock, so it must be short.
  - `_press_until_submitted(pane_id, text, first_pressed, busy_hint, baseline)` — Press Enter (each press under the typing mutex) until `text` leaves the box.
  - `_read_status(pane_id)` — Quiet status read: None on any probe failure (never fails a send).
  - `_turn_started(pane_id, pre_status)` — True when a turn STARTED during this send: working now, not working before.
  - `_remember_typed(pane_id, text)`
  - `_looks_like_ours(pane_id, shown)` — True if what is in the box is text we typed.
  - `_region_is_ours(pane_id, region)` — t130: STRICT ownership for the ambiguous-frame flush. The complete
  - `_composer_ownership(pane_id, screen)` — (owned, region) for an ambiguous frame - THE shared decision path.
  - `composer_region_is_owned(pane_id, screen)` — PUBLIC (t377 seam, Owner2282): the exact-owned full-composer check.
  - `_dim_bodies_accounted(pane_id, screen)` — Account the COMPLETE excluded span, never only a sigil's first row.
  - `_box_text(pane_id)` — Whatever is sitting in the pane's input box: '' when empty, None
  - `_recovery_box_text(pane_id, screen)` — Unknown geometry is never evidence that the prompt was consumed.
  - `press_keys(pane_id, keys)` — Press a sequence, under the global typing mutex (one burst).
  - `_press_keys_unlocked(pane_id, keys)` — Press keys in a pane. When the last key is Enter and the box holds
  - `submit_text(pane_id, text, press_enter, check_box)` — Type `text` into a pane, press Enter, and confirm it left the box.
  - `flush_box(target, self_pane_id)` — Submit whatever is sitting unsent in a pane's input box.
  - `_box_signature(pane_id)` — What the pane is showing, as ("modal"|"box", text). A panel and a
  - `_note_stale_panel(pane_id, signature, since)` — Say once that a dialog is holding up this pane. Never answer it.
  - `_draft_persists(pane_id)` — Read the box again after a moment and return what is still there.
  - `resolve_lanes()` — Every pane that has an agent in it: the set the dispatcher may feed.
  - `delivery_state(pane_id, status, pane, screen)` — Classify whether this pane can receive a message, and why not.
  - `clear_box(target, self_pane_id, reason)` — CLEAR an unsubmitted draft (Ctrl-U, Escape). The destructive pair to
  - `_ensure_box_clear(pane_id, allow_modal, preserve_draft)` — Make the input box ready to type into. Our own text stranded by a
  - `_lock_for(pane_id)`
  - `send_guarded(pane_id, send, last_sent_text, awaiting_consumption, hold_while_typing, allow_modal, preserve_draft, permission)` — Run `send` (a callable that types into `pane_id`) with the send lock
  - `_box_check(enabled, allow_modal, already_checked)` — Tell the typing primitive how this send treats the box.
  - `_typing(pane_id, timeout_s)` — The global typing mutex, held for one keystroke burst.
  - `_claude_busy(pane_id, pane)` — True only when Claude's own session reports status=busy AND that report
  - `_pane_is_quietly_idle(pane_id)` — True only when the pane itself says nothing is happening.
  - `_claim_names_a_dead_session(session)` — A sentence naming why a busy claim is WRONG rather than stale, or None.
  - `_human_typing(dunker)` — True if someone is composing text in the target's input box right
  - `_timer_loop(dunker, stop_event)`
  - `close()` — Halt every timer thread, keeping persisted running flags so a new
- `_intent_order(entry)` — Order two delivery intents: created_at, then the message id as a tie-break.
- `_hold_backoff_s(attempts)` — Exponential backoff for a refused delivery: base doubling, capped.
- `_looks_like_usage(text)` — herdr prints usage for a bare command group (exit non-zero); that is an
- `_describe(panes)`
- `_numeric_id(dunk_id)`
- `_check_only_when(value)`
- `_collapse(text)` — Whitespace-insensitive form of text, so wrapped lines still match.
- `_is_border_line(line)`
- `_bordered_input_area(tail)` — The pi TUI has no prompt sigil: its input is the line(s) between the
- `_input_area(tail)` — Return the agent's input-box region: everything from the last prompt
- `_input_pending(tail, last_sent_text)` — True if the target's input box still holds unconsumed input, False if it
- `_strip_ansi(text)` — Plain text of an ANSI-styled screen.
- `_sgr_dim(params, dim)` — Apply one SGR parameter list to the current dim state.
- `_bright_text(line)` — The characters of one ANSI-styled line that are not rendered dim.
- `_notice_text(mailbox, unread, sender, subject)` — The line typed into a pane to say mail has arrived.
- `_banner_marker(screen, markers)` — A marker that starts its own line is a banner; one mid-sentence is prose.
- `_is_placeholder(text)` — True when a line is the harness's empty-box hint rather than a state.
- `_status_marker(screen, markers, lines, placeholder_safe, dim_ok)` — A status decoration on the composer line: '... · Goal paused (/goal resume)'.
- `delivery_rows(screen)` — Non-blank rows with colour codes removed.
- `delivery_tail(screen, lines)` — The last non-blank lines of a pane, lowercased: where state banners live.
- `classify_delivery(screen, status, typed, our_text, awaiting_consumption)` — Why a pane can or cannot receive a message, as (state, reason).
- `_modal_open(screen)` — True if the pane is showing a panel or dialog *right now*.
- const `COMPOSER_HINT_TAIL_ROWS` = `5`
- const `BOX_REGION_ROWS` = `14`
- const `AMBIGUOUS_COMPOSER` = `'ambiguous composer/transcript:'`
- const `MID` = `' · '`
- const `BOX_LEADING` = `' \t│┃┆┊║|'`
- `_prompt_body(row)` — The text after the prompt sigil on one row, or None if the row has no sigil.
- const `NATIVE_BANNER_RE` = `re.compile('^>_\\s+OpenAI Codex(?:\\s+\\(v[0-9][0-9.]*\\))?$')`
- const `NATIVE_STATUS_RE` = `re.compile('^\\S[\\w .+/-]{0,40}\\s\\u00b7\\s(?:~|/)\\S*$')`
- const `NATIVE_HINTS_RE` = `re.compile('^(?:\\? for shortcuts|\\u2190 for agents)\\b')`
- const `NATIVE_WARNING_RE` = `re.compile('^\\u26a0\\s+\\d+\\s+warnings?\\s+\\u00b7\\s+f2 to view$')`
- `_native_banner(row)`
- `_native_status(row)`
- `_native_hints(row)`
- `_native_warning(row)`
- `_native_footer_start(raw, nonblank)` — The index of the native two-row footer's first row, or None.
- `_styled_sigil_state(row)` — Classify a prompt-sigil row by the SGR state IN EFFECT AT THE SIGIL
- `_styled_composer_boundary(screen)` — ("delimited", region) or ("undelimitable", "") using CAPTURED STYLE
- `_trailing_prompt_region(screen)` — Complete trailing region for the native-bound recovery path only.
- `_native_prefix_accounted(screen, native_text)` — An earlier prompt or human continuation cannot disappear at extraction.
- `_composer_region_text(screen)` — The live composer's visible content, or "" when it cannot be delimited
- `box_text(screen)` — What is in the input box, and how we know, as (text, detail).
- `_typed_text(screen)` — What a human has typed into the target's input box, judged from an
- `_is_our_send(typed, last_sent_text, awaiting_consumption)` — True if the text in the box is our own previous send rather than a
- `_check_bool(value, name)`
- `_check_max_sends(value)`

## dunk_desk.py (106 lines)

> A desk credential: exactly one permission, and it is not a superuser entry.  The web desk must be able to type into a lane the owner is looking at. Granting it

- const `CONFIG_DIR` = `Path(os.environ.get('DUNKINGSHEEP_DIR', str(Path.home() / '.config' / 'dunkingsheep')))`
- const `PATH` = `CONFIG_DIR / 'desk.json'`
- const `DESK_VERBS` = `frozenset({'send_text', 'send_keys'})`
- class **DeskError**(RuntimeError)
- `_read(path)`
- class **DeskCredential**
  - `__init__(path)`
  - `issue(panes)` — Create (or rotate) the token for this pane set. Returns the token once.
  - `panes()`
  - `has_token()`
  - `permits(command, target, token)` — True only for a listed verb on a listed pane with the right token.

## dunk_dialog_alerts.py (76 lines)

> Durable, deterministic notification for delivery-blocking dialogs.  Never sends keys or answers a prompt. Owner delivery uses the web desk directly,

- const `_LOCK` = `threading.Lock()`
- const `RETRY_S` = `60`
- `post_owner(body)`
- `notify(pane_id, generation, signature, owner_sink, vp_sink, path, now)` — Notify each sink per pane/process/dialog; retry failures after 60s.

## dunk_dispatch.py (604 lines)

> Deterministic feeding: a standby queue and a tick that fills idle lanes.  Every lane went idle and nothing noticed, so a human re-fed them by hand. This

- const `CONFIG_DIR` = `os.environ.get('DUNKINGSHEEP_DIR', os.path.expanduser('~/.config/dunkingsheep'))`
- const `DB_PATH` = `os.path.join(CONFIG_DIR, 'dispatch.db')`
- const `STATE_PATH` = `os.path.join(CONFIG_DIR, 'dispatch-state.json')`
- const `SCHEMA` = `'\nCREATE TABLE IF NOT EXISTS standby (\n    id INTEGER PRIMARY KEY AUTOINCREMENT,\n    ta`
- `_now()`
- class **DispatchError**(ValueError)
- class **Queue**
  - `__init__(path)`
  - `add(brief, task_id, lane)`
  - `get(item_id)`
  - `items(pending_only)`
  - `remove(item_id)`
  - `take(item_id, lane, sent_message, now)`
  - `next_for(lane)` — The first pending item assigned to this lane, else the first unassigned.
- `lane_plan(panes, delivery, unread, lanes)` — Classify every lane for one tick. Pure: everything is injected.
- const `PRIORITY_ORDER` = `{'urgent': 0, 'high': 1, 'medium': 2, 'low': 3}`
- const `HELD_PREFIX` = `'HELD FOR OWNER DECISION'`
- `derive_board_items(tasks, dispatched_task_ids, now, coarse_chars)` — Standby items DERIVED from the task board.
- `dispatch_text(item)` — What the lane is sent. One line, the task id first so the reply can cite it.
- const `TURN_END_MIN_GAP_S` = `float(os.environ.get('DUNKINGSHEEP_TURN_END_MIN_GAP_S', '30'))`
- const `TURN_END_TASK_COOLDOWN_S` = `float(os.environ.get('DUNKINGSHEEP_TURN_END_TASK_COOLDOWN_S', '300'))`
- `claimed_tasks(tasks, pane_id)` — The tasks `pane_id` has CLAIMED (holds a live lease on), in the order to offer them.
- `claimed_work(tasks, pane_id)` — The first task the lane has claimed (see `claimed_tasks`), or None.
- `turn_end_plan(pane_id, tasks, unread, last_nudge_at, task_nudges, now, min_gap_s, task_cooldown_s)` — Decide whether a turn end earns a prompt. Pure: everything is injected.
- `turn_end_text(task)` — What a lane that ended its turn holding work is sent.
- `tick(queue, panes, delivery, unread, send, now, dry_run, lanes, state_path, one_per_lane, derived)` — Fill idle lanes from the standby queue. Returns the state it wrote.
- `_dispatched_in_window(state_path, now, window_s)` — How many sends this hour, from the history this module keeps.
- `_load_state(path)`
- `_write_state(state, path)` — Atomic, and it keeps a small history so 'dispatched this hour' is real.
- `summary(state)` — The three numbers the owner asked for, one line each.
- `next_job_plan(trigger, pane_id, tasks, unread, pending_delivery, last_nudge_at, task_nudges, now, min_gap_s, task_cooldown_s, dispatch_reason)` — Decide what a lane does the moment it goes free. Pure: everything is injected.
- `assigned_open_tasks(tasks, pane_id)` — Open tasks ASSIGNED to `pane_id`, in board order, excluding blocked/gated ones.
- `delivery_retry_plan(attempt, last_at, now, max_attempts, min_gap_s)` — One bounded retry policy, with the dispatch rate floor and submit ceiling.

## dunk_fallback.py (370 lines)

> t359: deterministic model-fallback planning.  The owner's spec (i2249/i2250) asks for a function that, given the SAME credit

- const `TIER_ORDER` = `('claude', 'codex', 'pi', 'free')`
- const `REQUIRED_WINDOWS` = `{'claude': ('five_hour', 'weekly_all'), 'codex': ('weekly',), 'opencode-go': ('five_hour',`
- const `FREE_PROVIDER` = `'free'`
- const `TIER_PROVIDER` = `{'claude': ('claude',), 'codex': ('codex',), 'pi': ('opencode-go',), 'free': (FREE_PROVIDE`
- const `HARNESS_WIRING` = `{'codex': {'args': ['-s', 'danger-full-access', '-a', 'never'], 'emits_turn_events': True,`
- const `SPENT_PERCENT` = `0.1`
- const `RESERVE_PERCENT` = `5.0`
- class **FallbackError**(ValueError)
- `credit_state(readings)` — Reduce ``dunkingsheep usage --json`` output to {provider: state}.
- `tier_headroom(credit, tier)` — Lowest FINITE remaining percentage for a tier, or None.
- `tier_credit(credit, tier)` — The lowest remaining percentage reported for a tier, or None.
- `tier_state(credit, tier)` — Availability of one tier, from the reduced credit state.
- `_provider_label(tier)`
- `_credit_label(credit, tier)`
- `_reason_for_skip(reason)`
- `plan_swap(pane, credit, allowed_panes)` — Decide whether ``pane`` should be relaunched on another tier.
- `plan_all(panes, credit, allowed_panes)` — Plan every pane, in stable pane-id order so two readers agree.

## dunk_github.py (356 lines)

> GitHub Apps for the flat fleet: two bot identities, tokens minted on demand.  `docs/FLAT_FLEET.md` section 6. The owner has one GitHub login and no

- const `CONFIG_DIR` = `Path(os.environ.get('DUNKINGSHEEP_DIR', str(Path.home() / '.config' / 'dunkingsheep')))`
- const `API` = `os.environ.get('DUNKINGSHEEP_GITHUB_API', 'https://api.github.com')`
- const `KINDS` = `('author', 'reviewer')`
- const `DEFAULT_REPOS` = `('huntergdavis/dunkingsheep', 'huntergdavis/poolrad-decomp')`
- const `DEFAULT_REQUIRED_VERDICTS` = `1`
- const `REPO_REQUIRED_VERDICTS` = `{'huntergdavis/poolrad-decomp': 2}`
- const `REVIEW_CONTEXT` = `'dunkingsheep/review'`
- const `REFRESH_MARGIN_S` = `10 * 60`
- class **GitHubError**(RuntimeError)
- `_jwt_module()`
- `app_file(kind)`
- `load_app(kind)` — {"id", "slug", "pem"} for an app kind, or raise naming what is missing.
- `app_jwt(app, now)`
- `_request(method, url, token, bearer, payload, timeout)`
- class **Tokens**
  - `__init__(path)`
  - `_load()`
  - `_save()`
  - `installation_id(app, bearer)`
  - `get(kind, now)` — A live installation token for `kind`: {token, expires_at, kind, slug}.
  - `forget(kind)`
- const `ROLE_KIND` = `{'worker': 'author', 'planner': None, 'desk': None, 'reviewer': 'reviewer', 'system': 'rev`
- `kind_for_role(role, requested)` — Which app a role may act as. The owner and system may ask for either;
- `open_pulls(repo, token)`
- `pull_detail(repo, number, token)`
- `post_status(repo, sha, state, token, description, target_url)` — Commit status `dunkingsheep/review` on a head, as the reviewer App.
- `merge_pull(repo, number, token, sha)` — Merge an accepted PR as the reviewer App once branch protection is satisfied.
- `review_pull(repo, number, event, body, token)` — Approve or request changes on a PR as the reviewer App.
- `protection_payload(approvals, reviewer_app_id, bypass_app_slug)` — The branch-protection body for `main` (FLAT_FLEET.md 6.2).
- `required_verdicts(repo, config)` — How many DISTINCT reviewer lanes a head needs before it is final (t662).
- `repos_config()`

## dunk_guard.py (356 lines)

> The herdr guard: a `herdr` wrapper that routes typing through Dunking Sheep.  Dunking Sheep never types over someone who is mid-sentence, but that only

- const `CONFIG_DIR` = `os.environ.get('DUNKINGSHEEP_DIR') or os.path.expanduser('~/.config/dunkingsheep')`
- const `GUARD_DIR` = `os.path.join(CONFIG_DIR, 'bin')`
- const `GUARD_PATH` = `os.path.join(GUARD_DIR, 'herdr')`
- const `TMUX_GUARD_DIR` = `os.path.join(CONFIG_DIR, 'tmux-guard')`
- const `TMUX_WRAPPER_PATH` = `os.path.join(os.path.dirname(os.path.abspath(__file__)), 'dunk_tmux_guard.py')`
- const `BASHRC` = `os.path.expanduser('~/.bashrc')`
- const `MARKER` = `'# dunkingsheep herdr guard'`
- const `GUARD_MARKER` = `'dunkingsheep-herdr-guard'`
- const `REAL_SUFFIX` = `'-real'`
- const `WRAPPER_PATH` = `os.path.join(os.path.dirname(os.path.abspath(__file__)), 'dunk_guard_wrapper.py')`
- `_wrapper()` — The guard source, with the CLI path substituted.
- const `PATH_LINE` = `'export PATH="{guard_dir}:$PATH"'`
- const `LAUNCHER` = `'#!/bin/sh\n# Dunking Sheep launcher, installed by `dunkingsheep install`.\n# Keeps the CL`
- `install_launcher(bin_dir)` — Put `dunkingsheep` on PATH as an ordinary command.
- `_cli_path()`
- `_poll_path()`
- `resolve_real_tmux(guard_dir)` — Resolve an executable outside the guard chain, without launching it.
- `tmux_status()` — What the tmux guard looks like right now.
- `install_tmux()` — Install the tmux close/typing guard for the agent panes we start.
- `status()` — What the guard looks like right now.
- `install(shadow_binary)` — Write the wrapper and put its directory first on PATH for new shells.
- `_resolve_real_herdr()` — The real herdr on PATH, ignoring any guard wrapper.
- `_is_guard(path)`
- `shadow()` — Put the guard where herdr itself lives.
- `unshadow()` — Put the real herdr back in its own place.
- `_ensure_bashrc()`
- `uninstall()` — Remove the wrapper, restore herdr, and drop the PATH line.
- `check()` — Prove the guard intercepts: run the wrapper with a harmless command that

## dunk_guard_wrapper.py (182 lines)

> herdr guard - installed by `dunkingsheep guard install`; do not edit.  Routes pane-typing and destructive subcommands through the Dunking Sheep daemon

- const `DUNKINGSHEEP` = `__DUNKINGSHEEP__`
- const `GUARD_DIR` = `os.path.dirname(os.path.realpath(__file__))`
- const `NEWLINE` = `chr(10)`
- `is_guard(path)`
- `real_herdr()` — The actual herdr binary, never this wrapper. When the guard shadows
- `passthrough(argv)`
- `daemon_json(args, timeout)`
- `report_failure(text)`
- `route_close(kind, target)`
- `route_herdr(argv)`
- `route(kind, target, payload, argv)` — Hand a typing command to the daemon. A refused or unreachable daemon is
- const `MUTATING` = `(('pane', 'split'), ('pane', 'rename'), ('pane', 'focus'), ('tab', 'create'), ('tab', 'ren`
- const `CLOSING` = `(('pane', 'close', 'pane'), ('pane', 'kill', 'pane'), ('tab', 'close', 'tab'), ('workspace`
- `main(argv)`

## dunk_health.py (179 lines)

> Deterministic self-check and self-repair for the Dunking Sheep daemon.  There is no model in this loop. Every check has a fixed threshold and, when

- const `WAL_WARN_BYTES` = `64 * 1024 * 1024`
- const `DISPATCH_STALE_S` = `16 * 60`
- const `DISK_MIN_FREE_BYTES` = `512 * 1024 * 1024`
- const `HEALTH_PATH` = `os.path.expanduser('~/.config/dunkingsheep/health.json')`
- `_stale_leases(path, now)` — Count ingest rows a dead worker left behind. Read-only, fixed query.
- `_quick_check(path)`
- `_dispatch_freshness(config_dir, now)` — How long since a dispatcher tick completed: 'the feeder is dead' as a number.
- `run_health(flock, repair, deep)`
- `write_report(result, path)` — Write the last report where ops tooling (and the VP) can read it.

## dunk_help.py (759 lines)

> The one guide to Dunking Sheep, shared by every surface.  - `dunkingsheep guide` prints it (plain text or `--markdown`).

- const `SHORT` = `'Dunking Sheep types text (plus Enter) into terminal panes on a schedule.\nEach schedule i`
- `guide(markdown)` — Return the full guide as plain text (default) or GitHub markdown.
- `_underline(text, level)`

## dunk_http.py (290 lines)

> HTTP surface for the Command Center.  The UI needs a few shapes (roster, message state, reply stats, project summary,

- const `DEFAULT_PORT` = `int(os.environ.get('DUNKINGSHEEP_API_PORT', '8792'))`
- const `DEFAULT_HOST` = `os.environ.get('DUNKINGSHEEP_API_HOST', '127.0.0.1')`
- const `MAX_POST_BYTES` = `4096`
- class **ApiError**(Exception)
  - `__init__(status, message, context)`
- `error_body(error, message)` — The JSON a client sees for a refusal: error first, then only echoed context.
- `_client()`
- `_call(command)`
- `_unique_names(panes)` — Give every pane a name a caller can address it by: the agent label, made unique.
- `_lane_states()` — Pane status from the daemon, plus the reason when it cannot be had.
- `terminals()` — The tmux terminal list, read from the backend this host actually runs.
- `route(path, query)`
- `route_post(path, payload)` — The write half. Exactly one route exists, and it is named here, not guessed.
- `resize_terminal(name, payload)`
- `_now()`
- class **Handler**(BaseHTTPRequestHandler)
  - `do_GET()`
  - `do_POST()`
  - `log_message(fmt, *args)`
- `serve(host, port)`
- `main(argv)`

## dunk_inbox.py (888 lines)

> Per-agent inboxes: machine-to-machine messaging that never types.  Everything else in Dunking Sheep delivers by typing into a terminal's input

- const `CONFIG_DIR` = `os.environ.get('DUNKINGSHEEP_DIR') or os.path.expanduser('~/.config/dunkingsheep')`
- const `INBOX_DB` = `os.path.join(CONFIG_DIR, 'inbox.db')`
- const `BLOB_DIR` = `os.path.join(CONFIG_DIR, 'attachments')`
- const `RETENTION_S` = `7 * 24 * 3600`
- const `MAX_ATTACHMENT_BYTES` = `64 * 1024 * 1024`
- const `MAX_BLOB_BYTES` = `2 * 1024 * 1024 * 1024`
- const `MAX_BODY_BYTES` = `8 * 1024 * 1024`
- const `STATES` = `('queued', 'read', 'acked', 'cancelled', 'archived', 'undeliverable')`
- const `PANE_MAILBOX_RE` = `re.compile('^(?:w\\d+:p\\d+|%\\d+)$')`
- const `SCHEMA` = `'\nCREATE TABLE IF NOT EXISTS messages (\n    id            TEXT PRIMARY KEY,\n    mailbox`
- const `_PANE_SUFFIX` = `re.compile('\\s*\\(\\s*(?:[A-Za-z0-9_.-]+:p[A-Za-z0-9_.-]+|%\\d+)\\s*\\)\\s*$')`
- `normalise_mailbox(name)` — 'vp of agentry (w9:pH)' and 'vp of agentry' are ONE mailbox.
- `mailbox_where(mailbox, column)` — SQL that matches either spelling the table may hold for one mailbox.
- class **InboxError**(ValueError)
- `_now()`
- class **Inbox**
  - `__init__(path, blob_dir, retention_s)`
  - `close()`
  - `_next_id()` — Message ids are stable, never reused, and survive restarts: the
  - `ensure_next_job(message_id, mailbox, pane_id, body, subject)` — Materialise a task-store outbox intent once, including after a crash.
  - `send(mailbox, body, subject, sender, mailbox_label, pane_id, attachments)` — Put a message in a mailbox. Returns its row.
  - `upsert_digest(mailbox, digest_id, body, subject, sender, mailbox_label, pane_id)` — One message per digest id, updated in place.
  - `rename_mailbox(old, new)` — Move every message from one mailbox key to another. Returns how
  - `record_notify(message_id, state, reason, transport_id, attempt)`
  - `count(mailbox, lane_panes)`
  - `list(mailbox, unread_only, limit)` — Summaries, oldest first. No bodies.
  - `read(mailbox, limit, peek, mark)` — Full messages in queue order. Marks them read unless `peek` or `mark=False`.
  - `mark_read(message_ids, now)` — Commit the read for ids a caller has already rendered.
  - `record_delivery(mailbox, message_ids, now)` — Remember the latest delivered batch for a mailbox: the recovery buffer.
  - `pending_delivery(mailbox)` — The receipt for a mailbox, or None when nothing is recoverable.
  - `recover_delivery(mailbox)` — Replay the receipt batch for display, whatever its read state.
  - `note_acked(mailbox)` — Shrink the receipt to ids that are still not acked.
  - `get(message_id, with_body)`
  - `status(message_id)` — State of one message, by id, with no message text in it.
  - `stats()`
  - `ack(message_ids, mailbox)`
  - `cancel(message_ids)`
  - `archive(mailbox, before, states)` — Bulk-retire a mailbox without deleting anything: mark messages read
  - `mark_undeliverable(live_panes)` — A queued message whose resolved pane id no longer exists is
  - `auto_archive_undeliverable(age_s, now)` — Archive undeliverable mail whose target has been gone for `age_s`.
  - `archive_retired(mailboxes, age_s, now, reason, authorize, identity_guard)` — Archive mail addressed to a RETIRED pane's mailbox, keeping every row.
  - `_transition(message_ids, state, stamp, allowed, read_stamp, mailbox)`
  - `delete(message_ids)`
  - `purge(older_than_s)` — Drop read, acknowledged and cancelled mail past its retention.
  - `_store_blob(path)`
  - `fetch(attachment_id, dest_dir)` — Copy an attachment out of the store. Returns the written path.
  - `_blob_bytes()`
  - `_collect_blobs()` — Drop blobs no message references any more.
  - `_attachments(message_id)`
  - `_summary(row)`
  - `_full(row)`
- `_media_type(path)`

## dunk_load.py (254 lines)

> Per-caller CPU from /proc, with a pid+starttime guard and process trees.  Identity is the registered caller (pid + starttime). A recycled pid produces no

- const `TICKS` = `os.sysconf('SC_CLK_TCK') if hasattr(os, 'sysconf') else 100`
- const `STATE` = `Path(os.environ.get('DUNKINGSHEEP_LOAD_STATE', Path(os.environ.get('DUNKINGSHEEP_DIR', str`
- class **LoadError**(ValueError)
- `_stat(pid)` — Fields of /proc/<pid>/stat we need, or None when the process is gone.
- `_ppid_map()` — One walk: pid -> ppid for every visible process.
- `tree_of(pid, ppids)` — The caller plus descendants, via the ppid map. Sorted for stable output.
- `tree_cpu(pid, ppids)` — Summed utime+stime and RSS for the tree, or None if the root is gone.
- `registered_callers(tasks)` — [(name, pid, starttime)] from task leases that carry caller identity.
- `sample(callers, now)` — One sample: per caller, CPU ticks now and the pid-reuse verdict.
- `gpu()` — Per-process GPU here is a NULL with a reason, never a fabricated number.
- `_load_state(path)`
- `_save_state(state, path)`
- `collect(callers, now, policies, state_path)` — Sample and turn ticks into a window percentage of one core.

## dunk_mcp.py (306 lines)

> Dunking Sheep MCP server (stdio transport, standard library only).  Speaks JSON-RPC 2.0 over newline-delimited stdin/stdout, as the Model Context

- const `SUPPORTED_PROTOCOLS` = `('2025-11-25', '2025-06-18', '2025-03-26', '2024-11-05')`
- const `STRUCTURED_CONTENT_SINCE` = `'2025-06-18'`
- const `INSTRUCTIONS` = `'Dunking Sheep sends text (plus Enter) into herdr terminal panes on a schedule.\nEach sche`
- class **McpServer**
  - `__init__(client_factory, stdin, stdout)`
  - `_default_client()`
  - `serve_forever()`
  - `_serve_loop()`
  - `_write(payload)`
  - `handle(message)` — Return a response dict, or None for notifications.
  - `_error(msg_id, code, message)`
  - `initialize(params)`
  - `_role()` — This process's role, asked once: the list of tools follows it.
  - `list_tools()`
  - `call_tool(name, arguments)`
  - `_tool_error(message)`
- `main()`

## dunk_memory.py (1210 lines)

> Dunking Sheep memory and recall service (v1).  SQLite + FTS5 in WAL mode. One background writer ingests Claude/Codex/opencode/

- const `CONFIG_DIR` = `os.environ.get('DUNKINGSHEEP_DIR') or os.path.expanduser('~/.config/dunkingsheep')`
- const `MEMORY_DB` = `os.path.join(CONFIG_DIR, 'memory.db')`
- const `SCHEMA_VERSION` = `1`
- const `DEFAULT_CHUNK_CHARS` = `4000`
- const `DEFAULT_OVERLAP_CHARS` = `400`
- const `SCHEMA` = `"\nPRAGMA journal_mode = WAL;\nPRAGMA synchronous = NORMAL;\nPRAGMA foreign_keys = ON;\nPR`
- const `SECRET_PATTERNS` = `((re.compile('-----BEGIN [A-Z0-9 ]*PRIVATE KEY-----.*?-----END [A-Z0-9 ]*PRIVATE KEY-----'`
- `redact(text)` — Replace credential-shaped substrings with a stable marker.
- `chunk_text(text, max_chars, overlap_chars)` — Split text into overlapping chunks at whitespace boundaries.
- `fts_tokens(text)`
- `fts_query(text, operator)` — Safe FTS5 query: quoted tokens joined by operator (AND or OR).
- `make_snippet(text, tokens, max_chars)` — Cheap snippet from the stored text; do not use FTS5 snippet() in the
- `memory_sources()` — Known session sources, most recently modified first.
- `_harness_from_path(path)`
- `_project_from_cwd(cwd)`
- `_text_from_content(content)` — Extract text from a Claude/Codex/pi content value.
- class **MemoryStore**
  - `__init__(path, create)`
  - `_new_read_connection()`
  - `_read_connection()` — Reuse a pooled read-only connection; opening one per query
  - `_release_connection(conn)`
  - `close()`
  - `_begin_write()`
  - `_record_query(latency_ms, hits, error, cached)`
  - `_percentiles()`
  - `enqueue(path, harness, project)`
  - `checkpoint()` — Best-effort WAL checkpoint. PASSIVE never blocks on a live reader, so
  - `requeue_partial()` — Resume sources left partial by a deliberate stop (daemon restart).
  - `recover_leases()` — Requeue ingest leases whose worker died (expired lease).
  - `claim_next(lease_s)`
  - `finish(lease, ok, error, stats, skipped)`
  - `ingest_path(path, harness, project, should_stop, throttle_s)` — Parse one source. Returns counts. Never called from a query path.
  - `_upsert_source_row(path, harness, project)`
  - `_upsert_session(session_id, harness, project, cwd, title, started_at, updated_at, metadata)`
  - `_insert_chunks(session_id, chunks, role, kind, created_at, metadata)`
  - `_ingest_jsonl(path, harness, project, should_stop, throttle_s)`
  - `_ingest_codex_history(path, project, should_stop, throttle_s)`
  - `_ingest_opencode(path, project, should_stop, throttle_s)`
  - `recall(query, limit, harness, project, since, max_chars)`
  - `latest_session(harness, cwd)` — The most recently updated indexed session for a harness and cwd.
  - `context(query, sessions, snippets_per_session)`
  - `stats()`
  - `status()`
- `_iso_time(value)`
- `_event_time(event)`
- class **MemoryIngestor**
  - `__init__(store, poll_s, throttle_s)`
  - `start()`
  - `pause()`
  - `resume()`
  - `paused()`
  - `stop()`
  - `enqueue(path, harness, project)`
  - `status()`
  - `_loop()`

## dunk_metrics.py (247 lines)

> Operations telemetry: a durable, append-only record of what the daemon did.  Dunking Sheep already keeps two durable stores, but neither answers "how is

- const `CONFIG_DIR` = `os.environ.get('DUNKINGSHEEP_DIR') or os.path.expanduser('~/.config/dunkingsheep')`
- const `METRICS_DB` = `os.path.join(CONFIG_DIR, 'metrics.db')`
- const `RETENTION_S` = `30 * 24 * 3600`
- const `MAX_EVENTS` = `2000000`
- const `SCHEMA_VERSION` = `1`
- const `SCHEMA` = `'\nCREATE TABLE IF NOT EXISTS events (\n    id          INTEGER PRIMARY KEY AUTOINCREMENT,`
- const `KINDS` = `{'message.sent': 'text was typed into a pane and confirmed submitted', 'message.failed': '`
- const `OUTCOMES` = `{'ok': 'completed as intended', 'failed': 'attempted and did not complete', 'held': 'not a`
- const `REASONS` = `{'draft': 'someone was writing in the input box', 'dialog': 'a dialog was open, waiting fo`
- class **Metrics**
  - `__init__(path, retention_s, enabled)`
  - `close()`
  - `record(kind, pane_id, mailbox, sender, source, outcome, reason, duration_ms, attempts, bytes, at)` — Append one event. Never raises: telemetry must not break delivery.
  - `prune(older_than_s, max_events)`
  - `earliest()`
  - `_rows(start, end)`
  - `summary(start, end)` — Counts and duration distributions for one window.
- `_tally(rows, column)`
- `_histogram(values)`
- `_distribution(rows, kind)`
- `_percentile(sorted_values, fraction)` — Nearest-rank percentile. With few samples an interpolated figure would

## dunk_native_session.py (327 lines)

> Fail-closed native-session activity guard for same-session Codex resume.  The guarded same-pane respawn replaces a pane's process; it does not stop a

- class **NativeSessionRefusal**(RuntimeError)
- class **NativeSessionChanged**(NativeSessionRefusal)
- class **NativeSessionBusy**(NativeSessionRefusal)
- class **ResumeRequest**
- class **NativeSessionSnapshot**
  - `assert_unchanged()` — Refuse if the exact rollout changed after this snapshot was read.
- `_signature(stat)`
- `find_resume_request(argv)` — Return the exact Codex resume request in an argv, or None.
- `_state_row(codex_home, session_id)`
- `_read_rollout(path, session_id, expected_cwd)` — Parse a rollout and return (signature, started, completed, active).
- `check_native_session_quiescence(argv, pane_cwd)` — Return a native-session snapshot or refuse.

## dunk_next_job.py (285 lines)

> t372 execution wiring. Tasks journal intentions; inbox owns delivery receipts.  No new store, no terminal typing bypass, and no polling of unassigned backlogs.

- class **DispatchPaused**(Exception)
- class **NextJobs**
  - `__init__(flock)`
  - `_pane_lock(pane)`
  - `gate(pane, lanes)` — Use the existing dispatch kill switch and lane opt-in resolver.
  - `advance(pane, trigger, from_task, caller, operator_proof)`
  - `materialise(h)`
  - `recover()` — Startup uses the same decision; subsequent ticks service only intents.
  - `tick(restart)`
  - `deliver(h, restart)`
  - `observe_turn(pane, turn)` — Persist the producer's exact generation without claiming delivery.
  - `bind_turn(pane)` — Link a confirmed message to a new producer generation, never busy status.

## dunk_owner.py (222 lines)

> Direct owner delivery: the message is in front of the agent, nothing is typed.  The owner waited seven minutes for a message because it was written to an inbox and

- const `CONFIG_DIR` = `os.environ.get('DUNKINGSHEEP_DIR', os.path.expanduser('~/.config/dunkingsheep'))`
- const `DB_PATH` = `os.path.join(CONFIG_DIR, 'owner-push.db')`
- const `SCHEMA` = `'\nCREATE TABLE IF NOT EXISTS owner_push (\n    id INTEGER PRIMARY KEY AUTOINCREMENT,\n   `
- const `_SHARED` = `{}`
- const `_SHARED_LOCK` = `threading.Lock()`
- `shared(path)` — ONE connection for the daemon, behind the store's own lock.
- class **OwnerPush**
  - `__init__(path)`
  - `__enter__()`
  - `__exit__(*exc_info)`
  - `close()` — Close the connection. A caller that opens a store per query must be able
  - `push(pane_id, body, target, subject, now)`
  - `receipts(push_ids)` — READ-ONLY arrival receipts for specific pushes (t66, %33's request).
  - `take(pane_id, by, limit, now)` — Hand over what is waiting for this pane, oldest first.
  - `poll(pane_id, by, now)` — Record a successful poll by the lane's own harness (empty polls count).
  - `last_poll(pane_id)` — When this pane's harness last polled, or None. THE drain fact.
  - `last_take(pane_id)` — When this pane last drained a push, or None. The OBSERVED drain fact.
  - `pending(pane_id)`
  - `stats(window_s, now)`
- `_pct(values, q)`

## dunk_panes.py (201 lines)

> Retired panes: the ones we deliberately stopped feeding.  A pane can be superseded (a replacement was started elsewhere) or replaced by

- const `CONFIG_DIR` = `os.environ.get('DUNKINGSHEEP_DIR', os.path.expanduser('~/.config/dunkingsheep'))`
- const `PATH` = `os.path.join(CONFIG_DIR, 'retired-panes.json')`
- const `PANE_IN_MAILBOX` = `re.compile('([A-Za-z0-9_.-]+:p[A-Za-z0-9_.-]+)')`
- class **RetireError**(ValueError)
- `_load(path)`
- `identity_guard(paths, exclusive)` — Coordinate managed identity writers with a guarded archival transaction.
- `_write(data, path)`
- `_write_locked(data, path)`
- `retire(pane_id, reason, by, labels, mailboxes, path, now)` — Mark a pane retired. The reason is required: an unexplained silence is a bug.
- `unretire(pane_id, path)`
- `all_retired(path)`
- `is_retired(pane_id, path)`
- `mailbox_retired(mailbox, path)` — True when a mailbox belongs to a retired pane, however it is spelled.
- const `_ARCHIVE_PANE` = `'(?:[A-Za-z0-9_.-]+:p[A-Za-z0-9_.-]+|%\\d+)'`
- const `_ARCHIVE_SUFFIX` = `re.compile('\\s*\\((' + _ARCHIVE_PANE + ')\\)\\s*$')`
- `valid_retirements(records)` — A reasonless or undated record cannot withdraw identity protection.
- `retirement_aliases(record)`
- `retired_mailboxes(candidates, records, live_panes, protected)` — Exact database keys with positive retirement evidence, safe to age-archive.

## dunk_pm.py (170 lines)

> Project PMs: lane->PM routing and the one-per-project digest.  A PM does not get a stream of reports. It publishes ONE digest per project per

- const `DEFAULT_VP_PANE` = `os.environ.get('DUNKINGSHEEP_VP_PANE', 'w9:pH')`
- const `ROUTING` = `Path(os.environ.get('DUNKINGSHEEP_PM_ROUTING', Path.home() / '.config' / 'dunkingsheep' / `
- class **PmError**(ValueError)
- `load_routing(path, vp_pane)` — Return {project: {pm, members, mode, vp_pane}}. Empty if no file.
- `project_for(pane_id, routing)` — The project whose member list contains this pane, if any.
- `redirect(pane_id, routing, vp_pane)` — Decide where a lane's message to the VP should actually go.
- const `POOLRAD_CHECKPOINT` = `Path(os.environ.get('POOLRAD_CHECKPOINT', '~/workspace/poolrad-decomp/docs/re/function_acc`
- `checkpoint_numbers(project, path)` — Read the project's numbers AT PUBLISH TIME. None = no source for this project.
- `render_digest(project, changed, blocked, owner_decisions, numbers, window, note)` — Render the four-section digest. Empty sections say so, never vanish.
- `digest_id(project)`
- `parse_payload(raw)` — Accept a JSON digest payload or a plain body string.

## dunk_policy.py (149 lines)

> Per-agent resource policy: alerts, conservative auto-kill, and a deny list.  Measurement comes from the #1252 sampler (dunk_load), so this module never

- const `DEFAULT_GRACE_S` = `30.0`
- class **PolicyError**(ValueError)
- `_num(policy, key, default)`
- `evaluate(sample, policy, now)` — Findings for one caller sample against one agent's policy.
- `denied(pid, deny_pids, deny_panes, workspace_roots, cwd, protected_panes)` — The deny evaluation. Returns a reason string, or None when permitted.
- `kill_decision(sample, policy, registered, deny_pids, deny_panes, workspace_roots, protected_panes, dry_run, grace_s, signal_fn, sleep_fn, now)` — Decide and (unless dry_run) act on one over-threshold caller.

## dunk_poolrad.py (203 lines)

> Poolrad backlog-sync and progress (FLAT_FLEET.md 6.5): the queue fills itself.  The VP and CoS used to write every claimable Poolrad body by hand. This is the

- const `CONFIG_DIR` = `Path(os.environ.get('DUNKINGSHEEP_DIR', str(Path.home() / '.config' / 'dunkingsheep')))`
- const `DEFAULT_REPO` = `os.environ.get('DUNKINGSHEEP_POOLRAD_REPO', '/home/hunter/workspace/poolrad-decomp')`
- const `CEILING` = `815`
- const `QUEUE_JSON` = `CONFIG_DIR / 'poolrad' / 'strict-queue.json'`
- const `EXPORT_ARGS` = `['--inventory', 'resources/function-accounting-current/inventory.json', '--callgraph', 're`
- const `STRICT_MANIFEST` = `'docs/re/strict_full_c_addresses.csv'`
- const `IDENTITIES` = `'docs/re/reviewed_function_identities.csv'`
- const `METHOD_DOC` = `'docs/re/STRICT_CANDIDATE_QUEUE.md'`
- const `TITLE_PREFIX` = `'Poolrad strict C:'`
- `_git(repo, *args)`
- `credited_ids(repo)`
- `identity_count(repo)`
- `export_queue(repo, out, timeout)` — Run the repo's exporter read-only; the JSON lands in our state dir.
- `candidates(queue, credited, filed_ids, limit)` — Ranked candidates worth filing: composable, not credited, not filed.
- `task_title(row)`
- `task_body(row, base, repo)`
- `filed_ids_from_tasks(tasks)`
- class **Sync**
  - `__init__(flock, repo, exporter)`
  - `run(limit, dry_run, priority)`
- `progress(repo, queue_path)`

## dunk_recovery.py (136 lines)

> t504: bounded, deterministic recovery for a DS-owned prompt in an ambiguous frame.  The measured stall: d82 typed its own prompt into %2 at 08:28 and the lane never

- const `RECOVERABLE_ATTEMPT_STATES` = `frozenset({'pending', 'unconfirmed', 'typed'})`
- `_norm(text)` — Compare what a person sees, not what a terminal wrapped.
- `_refuse(reason)`
- `plan_recovery(attempt, frame, pane, native_session_text, recoveries_done, max_recoveries)` — Decide whether to submit an owned, stalled prompt once more.

## dunk_recovery_native.py (149 lines)

> Process-bound Codex evidence for owned-prompt recovery (t508).  No newest-file/session inference: the running Codex process must name an exact

- const `_CACHE` = `OrderedDict()`
- const `_CACHE_LOCK` = `threading.Lock()`
- class **PromptSession**
  - `assert_unchanged()`
- `_process_environment(pid)`
- `_registered_binding(pane, pid, start)` — Opt-in operator binding for legacy bare Codex launches.
- `resolve_prompt_session(backend, pane)` — Resolve identity only: safe to call before Enter without reading history.
- `read_prompt_session(backend, pane)` — Read one kernel-bound Codex session or raise; never guess a session.

## dunk_review.py (424 lines)

> The review timer: GitHub PRs -> review tasks -> commit status (FLAT_FLEET.md 6.3).  Deterministic, no model. Every two minutes:

- `attribute_author(pr, lanes, mints)` — Pane id of the lane that opened `pr`, or None.
- `_iso(value)`
- `review_body(pr, detail, author)`
- class **Sync**
  - `__init__(flock, tokens, github)`
  - `_live_reviews()`
  - `_judged_heads()` — (repo, pr, head) -> verdict for every DONE review: a head that was
  - `head_verdicts(repo, head, number)` — The verdicts recorded at one head, by DISTINCT lane (t662).
  - `_is_final(repo, head, number, lane)` — Is this head final: enough DISTINCT accepting lanes and no needs-fix?
  - `_second_review(repo, number, head, first_lane, report)` — File the second review for a head whose first verdict was an accept (t662).
  - `run(dry_run)`
  - `_merge(repo, number, head, token, report, task_id)`
  - `_status(repo, sha, state, token, description, url, report)`
  - `on_verdict(task, verdict, by)` — Called by the daemon when a review completes: status + approval + follow-up.
  - `_standing_accept(task)` — The task id of another DONE review that accepted this exact head, or None.
  - `_evidence(task)` — The reviewer's own notes, verbatim, or a sentence saying there are none.
  - `_review_body(task, by, head, evidence)`

## dunk_roles.py (415 lines)

> Roles: who a caller is, bound to a process, and what that role may call.  Replaces the superuser allowlist (`dunk_superusers.py`) and the desk credential

- const `CONFIG_DIR` = `Path(os.environ.get('DUNKINGSHEEP_DIR', str(Path.home() / '.config' / 'dunkingsheep')))`
- const `ROLES_FILE` = `str(CONFIG_DIR / 'roles.json')`
- const `TOKENS_FILE` = `str(CONFIG_DIR / 'service-tokens.json')`
- const `ROLES` = `('owner', 'system', 'web', 'desk', 'planner', 'worker', 'reviewer', 'anonymous')`
- const `LANE_ROLES` = `('desk', 'planner', 'worker', 'reviewer', 'owner')`
- const `TOKEN_ROLES` = `('web', 'system', 'owner')`
- const `DEFAULT_ROLE` = `'anonymous'`
- const `UNCONFIGURED_DEFAULT` = `'worker'`
- const `DEFAULT_REVIEWER_PROVIDERS` = `('claude', 'codex')`
- const `READ_ONLY` = `frozenset({'status', 'poll', 'health', 'help', 'doctor', 'whoami', 'roster', 'message_stat`
- const `_WORKER` = `READ_ONLY | frozenset({'task_next', 'task_heartbeat', 'task_release', 'task_complete', 'ta`
- const `ALLOW` = `{'owner': None, 'system': READ_ONLY | frozenset({'task_add', 'task_assign', 'task_cancel',`
- const `WEB_DENIED` = `frozenset({'close', 'shutdown', 'respawn_pane', 'start_agent', 'create_workspace', 'create`
- const `DESK_LIMIT` = `5`
- const `DESK_RESULT_BYTES` = `4096`
- class **RoleError**(RuntimeError)
- `allowed(role, command)` — May `role` call `command`?
- `_hash(token)`
- `_read_json(path)`
- `_write_json(path, payload)`
- `_stamp(path)`
- class **Roles**
  - `__init__(path, tokens_path, default)`
  - `refresh(force)`
  - `save()`
  - `_save_tokens()`
  - `register(pane_id, role, pane_pid, start, provider, model, by, label)`
  - `unregister(pane_id)`
  - `entry(pane_id)`
  - `owners()`
  - `has_owner()`
  - `role_of(pane_id, pane_info)` — The registered role for a pane, or the default.
  - `provider_of(pane_id)`
  - `may_review(pane_id, role)` — Reviewer role AND a provider on the reviewer list (free models never review).
  - `issue_token(role, label, by)`
  - `revoke_token(label)`
  - `token_role(token)`
  - `tokens()`
  - `describe()`
- `clamp_desk_args(command, args)` — Daemon-side result caps for the desk role (FLAT_FLEET.md 4.3).
- `clamp_desk_result(result)` — Truncate a result the desk could not read in time anyway.

## dunk_server.py (400 lines)

> Dunking Sheep daemon: owns the `Flock` and serves it over a unix socket.  Protocol (newline-delimited JSON, any number of requests per connection):

- const `DEFAULT_SOCKET` = `os.path.join(CONFIG_DIR, 'dunkingsheep.sock')`
- const `SOCKET_PATH` = `os.environ.get('DUNKINGSHEEP_SOCKET') or DEFAULT_SOCKET`
- const `LOG_FILE` = `os.path.join(CONFIG_DIR, 'daemon.log')`
- class **AlreadyRunning**(RuntimeError)
- class **FlockServer**
  - `__init__(flock, socket_path)`
  - `bind()`
  - `serve_forever()`
  - `shutdown()`
  - `close()`
  - `_serve_connection(conn)`
  - `_peer_pid(conn)` — The connecting process, from SO_PEERCRED (Linux); None elsewhere.
  - `_bind_identity(claimed, caller, command)` — The pane a request acts as is the pane its process runs in, or none.
  - `handle_line(line, peer_pid)` — Return (response dict, stop_after).
- `_socket_alive(path)` — True if something accepts connections on the unix socket at `path`.
- `configure_logging(to_file, level)` — Log to the daemon log file, plus stderr when it is a terminal (a detached
- `kill_own_process_group(reason)` — SIGTERM the daemon's whole process group (t212).
- `run_daemon(socket_path, state_file, log_to_file)` — Blocking entry point used by `dunkingsheep serve`.

## dunk_sqlite.py (30 lines)

> Consistent SQLite connections for daemon stores and read-only consumers.

- const `BUSY_TIMEOUT_MS` = `5000`
- `connect(path, readonly, check_same_thread, timeout)` — Use WAL for owned stores; never change journal mode on external reads.

## dunk_state.py (602 lines)

> Continuous agent-state capture: what it would take to put a lane back.  The owner asked for every agent to be restartable by the VP with its previous state,

- const `CONFIG_DIR` = `os.environ.get('DUNKINGSHEEP_DIR', os.path.expanduser('~/.config/dunkingsheep'))`
- const `DB_PATH` = `os.path.join(CONFIG_DIR, 'agent-state.db')`
- const `SCHEMA` = `'\nCREATE TABLE IF NOT EXISTS agent_state (\n    at REAL NOT NULL,\n    pane_id TEXT NOT N`
- const `_MODEL_FLAGS` = `('--model', '-m')`
- const `_THINKING_FLAGS` = `('--thinking', '--reasoning', '--effort')`
- class **StateStore**
  - `__init__(path)`
  - `_migrate()` — Add columns an older database is missing, without losing its rows.
  - `add(rows, at)`
  - `latest(pane_id, project, limit)`
- `project_of(cwd)` — The project label: the workspace directory name, or null.
- const `_SESSION_FLAGS` = `('--session', '--resume')`
- const `_AGENT_EXES` = `('claude', 'pi', 'codex', 'opencode')`
- const `_SESSION_ROOTS` = `(os.path.expanduser('~/.pi/agent/sessions'), os.path.expanduser('~/.claude/projects'), os.`
- `_proc_children()` — pid -> [child pids], from /proc. Linux only; an empty map elsewhere.
- `_proc_snapshot()` — One /proc pass: (pid -> [children], pid -> process start time).
- `_proc_start_ticks(pid)` — Raw start-time ticks of a pid (field 22 of stat), or None.
- `_read_bound(pid, starts, reader)` — Read process metadata only when the identity holds BEFORE and AFTER.
- `_still_the_same_process(pid, starts)` — Is `pid` still the process the snapshot saw? Unknown is NOT the same.
- `_descendants(pid, kids)`
- `_cmdline(pid)`
- `_open_session_file(pid)` — The session file this process has open, if any. Identity from the PROCESS, not the cwd.
- `_session_id_from_path(path)`
- `_env_of(pid)`
- `_proc_start(pid)` — Wall-clock start time of a pid, from /proc/<pid>/stat starttime + boot time.
- `_active_session_file(cwd, since, slack)` — The session file this workspace is actually being written to.
- `_cwd_slug(cwd)` — The sessions-directory slug a harness uses for a working directory.
- `session_of(pane_pid, launch, kids, cwd, starts)` — (session_id, reason) for a live pane, read from its agent PROCESS.
- `_flag(argv, names)`
- `capture(flock, source, now, store)` — One capture: a row per agent lane, fields from argv/status/activity.

## dunk_stuck.py (356 lines)

> Deterministic recovery of a stuck agent message (t377, owner2281/2282).  THE PROBLEM. A message the daemon typed can sit unsubmitted in a lane's composer

- const `SCHEMA_FIELDS` = `('message_id', 'generation', 'fingerprint', 'first_seen', 'last_attempt_at', 'attempts', '`
- const `ACTIONS` = `('wait', 'recover', 'cooling', 'reset', 'terminal')`
- const `DEFAULT_THRESHOLD_S` = `120.0`
- const `DEFAULT_MAX_ATTEMPTS` = `2`
- const `DEFAULT_COOLDOWN_S` = `60.0`
- const `DEFAULT_OBSERVATION_INTERVAL_S` = `20.0`
- const `BLOCKED_STATUSES` = `('blocked', 'dead', 'unknown', 'working')`
- const `_WS` = `re.compile('\\s+')`
- `collapse(text)` — The composer's own wrapping is not content: compare on collapsed text.
- `fingerprint(text)` — Stable identity of the COMPLETE composer text.
- `_positive(raw, default, what)`
- class **Config**
  - `from_mapping(cls, raw)`
- class **Observation**
  - `key()`
- class **Decision**
  - `__post_init__()`
  - `recovers()`
- class **Tracker**
  - `from_dict(cls, data)`
  - `to_dict()`
  - `ledger_key(pane_id, message_id, generation, fingerprint)`
  - `reserved(pane_id, message_id, generation, fingerprint)`
  - `dump()`
  - `as_state()` — The shape stored in the daemon's state file (both halves).
  - `from_state(cls, data)`
  - `load(cls, text)`
  - `_reset(obs, reason)`
  - `decide(obs, config, now)` — What to do about this observation, and the record it leaves behind.
  - `mark_terminal(pane_id, reason)`
  - `forget(pane_id)`
- `obs_now(now)`
- `verify(before, after)` — Independent consumption evidence after ONE Enter. Returns (outcome, detail).

## dunk_superusers.py (288 lines)

> The superuser allowlist for destructive Dunking Sheep operations.  Only panes/agents on this list may close panes, tabs or workspaces, stop the

- const `CONFIG_DIR` = `os.environ.get('DUNKINGSHEEP_DIR') or os.path.expanduser('~/.config/dunkingsheep')`
- const `SUPERUSERS_FILE` = `os.path.join(CONFIG_DIR, 'superusers.json')`
- const `DEFAULT_VP_NAME` = `os.environ.get('DUNKINGSHEEP_VP_NAME') or 'VP of agentry'`
- const `DEFAULT_OWNER_NAME` = `os.environ.get('DUNKINGSHEEP_OWNER_NAME') or 'owner'`
- class **SuperuserError**(ValueError)
- `_classify(entry)`
- `_lock_path(path)` — Sidecar lock file beside the allowlist (same directory/filesystem).
- `_locked(path, exclusive)` — Serialize allowlist writers against authorization readers (t423).
- `locked_allowlist(path)` — Hold a shared allowlist lock across check-through-mutation (t423).
- `_parse_allowlist(data)` — (panes, names) sets from decoded JSON; shared by load and locked reads.
- `match_allowlist(panes, names, pane_id, pane_info)` — Pure membership test shared by cached and freshly-read sets (t423).
- `read_panes_locked(path)` — (panes, names) re-read from disk under a shared lock (t423).
- class **Superusers**
  - `__init__(path, create)`
  - `_file_stamp()`
  - `refresh()` — Re-read the allowlist if the file changed under us.
  - `load(create)`
  - `read_panes_locked()` — (panes, names) re-read from disk under a shared lock (t423).
  - `seed_defaults()` — First run: the VP name, the owner name, and any panes the setup set.
  - `save()`
  - `entries()`
  - `empty()`
  - `is_superuser(pane_id, pane_info)`
  - `describe()`
  - `add(entry)`
  - `remove(entry)`

## dunk_tasks.py (1720 lines)

> Global and per-pane tasks: the coordination backbone beyond context limits.  A task is a row in a SQLite database owned by the daemon, addressed through the

- const `CONFIG_DIR` = `os.environ.get('DUNKINGSHEEP_DIR') or os.path.expanduser('~/.config/dunkingsheep')`
- const `TASKS_DB` = `os.path.join(CONFIG_DIR, 'tasks.db')`
- const `GLOBAL` = `'global'`
- const `STATUSES` = `('open', 'in_progress', 'blocked', 'done', 'cancelled')`
- const `PRIORITIES` = `('low', 'medium', 'high', 'urgent')`
- const `PRIORITY_RANK` = `{'urgent': 0, 'high': 1, 'medium': 2, 'low': 3}`
- const `DEFAULT_STATUS` = `'open'`
- const `DEFAULT_PRIORITY` = `'low'`
- const `DEFAULT_LEASE_S` = `float(os.environ.get('DUNKINGSHEEP_LEASE_S', '1800'))`
- const `_PANE_ID` = `re.compile('^(?:[A-Za-z0-9_.-]+:[A-Za-z0-9_.-]+|%\\d+)$')`
- const `KINDS` = `('work', 'review', 'verify', 'report')`
- const `DEFAULT_KIND` = `'work'`
- const `VERDICTS` = `('accept', 'needs-fix')`
- const `SCHEMA` = `"\nCREATE TABLE IF NOT EXISTS tasks (\n    id            TEXT PRIMARY KEY,\n    scope     `
- class **TaskError**(ValueError)
- `_now()`
- `_as_list(value)`
- `_decode(raw)`
- `_encode(values)`
- `validate_scope(scope)`
- `validate_status(status)`
- `validate_kind(kind)`
- `validate_verdict(verdict)`
- `validate_priority(priority)`
- class **Tasks**
  - `__init__(path)`
  - `_connect()` — This thread's connection, opened on first use and CLOSED when the thread ends.
  - `_prune_locked(current)` — Close connections whose owning thread has exited. Caller holds the lock.
  - `connection_count()` — How many connections are open right now, WITHOUT pruning.
  - `__getattribute__(name)`
  - `_migrate()` — Add columns introduced after the first database was created.
  - `close()` — Close every thread's connection (the store is shared by the daemon).
  - `_next_id()`
  - `record_next_job_handoff(task_id, pane_id, from_task, message_id, attempt, reason, now, details)` — Record that `pane_id` was handed `task_id` after completing `from_task` (t372).
  - `_fire(task_id, event)` — Run `on_event` callbacks after a commit. Callbacks must be cheap and
  - `_record(task_id, event, at, actor, scope, priority, status_from, status_to, duration_ms, meta)` — Append one immutable event. Metrics are computed from these rows,
  - `_statuses(ids)`
  - `_decorate(row, pane_id)`
  - `_full(row, pane_id)`
  - `_order_key(task)`
  - `_require(task_id)`
  - `add(scope, title, body, status, priority, assignees, parent_id, depends_on, created_by, multi_assignee, kind, author, repo, pr, head)`
  - `update(task_id, title, body, status, priority, parent_id, assignees, depends_on, block_reason, updated_by, multi_assignee)` — Change any mutable field. ``...`` (Ellipsis) means "leave alone".
  - `assign(task_id, assignees, mode)`
  - `complete(task_id, by, verdict)`
  - `set_verdict(task_id, verdict, by)` — Record a review verdict. Written by the daemon from the bound reviewer
  - `notes(task_id, event)` — Every note event on a task, oldest first: [{at, actor, meta, text}].
  - `next_job_handoffs(pane_id)` — Latest durable state of each handoff, including terminal receipts.
  - `active_handoffs(pane_id, owned_task_ids)` — Handoffs that still count as "a prompt is on its way" for this pane.
  - `advance_next_job(pane_id, trigger, from_task, caller, unread, pending_delivery, dispatch_reason, operator_proof)` — Complete A, choose/claim B and journal its inbox id in ONE transaction.
  - `update_next_job(handoff, expected)` — Append progress under the writer lock, preserving the immutable chain.
  - `block(task_id, reason)`
  - `unblock(task_id)`
  - `cancel(task_id, reason)`
  - `delete(task_id)`
  - `note(task_id, event, actor, meta)` — Append a history event without changing the task row (or needing it).
  - `get(task_id, pane_id)`
  - `_visible_scopes(scope, self_pane_id)` — Which scopes a caller may see.
  - `list(scope, status, assignee, include_done, limit, self_pane_id, kind, needs_owner)`
  - `next(scope, self_pane_id)` — The next eligible task in the global queue and in a pane queue.
  - `_rows(sql, params)` — Rows as sqlite3.Row, whatever the connection's factory says today.
  - `leases_for(task_id, now)` — Live leases for a task (expired rows are ignored).
  - `leases_for_pane(pane_id, now)` — Live leases held by a pane.
  - `_claimable(task, pane_id)` — Whether ``pane_id`` may take this task right now.
  - `_busy_task_ids(pane_id, now)` — Ids of in-progress tasks this pane holds a live lease on.
  - `_refusal(candidates, pane_id)` — Why nothing was claimable, as a reason plus the evidence for it.
  - `_log_refusal(pane_id, target, refused)`
  - `claim_next(scope, pane_id, lease_seconds, caller, allow_busy, kinds, exclude_author)` — Atomically claim the next eligible task in ``scope``.
  - `_claim_locked(chosen, pane_id, lease_s, caller, now)` — Shared atomic lease write; caller owns BEGIN IMMEDIATE.
  - `heartbeat(task_id, pane_id, lease_seconds, caller)` — Extend a live lease. Raises when the caller does not hold one.
  - `release(task_id, pane_id)` — Give a claim back without completing it.
  - `reap(alive_panes, caller_alive, now, alive_source)` — Return expired, dead-pane or dead-caller leases to the queue.
  - `_after_lease_change(task_id, raw_assignees, now, pane_id, row)` — Reopen a task when its last lease goes away; keep the OWNER.
  - `stats(scope, self_pane_id)`
  - `metrics(window_s, since, until, scopes, bucket, scope_label)` — Timeseries-friendly task metrics from the append-only event log.
  - `for_pane(pane_id, limit)` — The block inbox_read appends: global + own-pane open work + next.

## dunk_telegram_routing.py (38 lines)

> Shared Telegram routing reader; never opens bot credentials or pairing data.

- `resolve_telegram_channel(data_dir, channels_root)` — Resolve environment > local routing config > cos, validating desk IDs.

## dunk_terminal.py (914 lines)

> Terminal adapter seam.  Everything above this module (Flock, the task/inbox stores, the CLI, MCP)

- const `DEFAULT_TMUX_REGISTRY` = `os.path.join(os.environ.get('DUNKINGSHEEP_DIR') or os.path.expanduser('~/.config/dunkingsh`
- class **TerminalBackend**
  - `capabilities()`
  - `tab_labels()`
  - `list_panes_with_tabs()`
  - `list_panes_grouped()`
  - `agent_status(pane_id)`
  - `agent_session(pane_id)`
  - `self_pane_id()`
  - `resize(pane_id, cols, rows)` — Backends that cannot size a pane say so instead of pretending.
- class **HerdrBackend**(TerminalBackend)
  - `__init__(client)`
  - `client()`
  - `__getattr__(item)`
  - `capabilities()`
  - `tab_labels()`
  - `list_panes_with_tabs()`
  - `list_panes_grouped()`
  - `agent_status(pane_id)`
  - `agent_session(pane_id)`
- `_safe_name(label, fallback)` — tmux session/window names cannot contain `:`, `.` or whitespace.
- `_agent_kind(command, title)` — Best-effort agent kind from tmux's foreground command and pane title.
- class **TmuxBackend**(TerminalBackend)
  - `__init__(socket_name, registry_path, tmux_bin, guard_dir)`
  - `capabilities()`
  - `_run(args, timeout)`
  - `_tmux(*args, timeout)`
  - `version()`
  - `is_available()`
  - `server_error_hint()`
  - `_load_registry()`
  - `_save_registry(data)`
  - `_remember(kind, item)`
  - `_forget(kind, item_id)`
  - `_managed(kind, item_id, panes)`
  - `_parse_panes(out)`
  - `list_panes()`
  - `get_pane(pane_id)`
  - `agent_child_pids(pane_pid)` — Kernel process entries for the agent behind `pane_pid` (t215).
  - `list_tabs()`
  - `list_workspaces()`
  - `_env_args(env)`
  - `_session_target(workspace_id)`
  - `create_workspace(label, cwd, focus, env)`
  - `create_tab(workspace_id, label, cwd, focus, env)`
  - `split_pane(pane_id, direction, cwd, ratio, focus, env, command)`
  - `_agent_command(argv)`
  - `_agent_launch(command)` — Wrap an agent command so its exit is visible rather than invisible.
  - `start_agent(name, argv, workspace_id, tab_id, split, cwd, focus, env)`
  - `respawn_pane(pane_id, argv, expected_pid, expected_start)` — Replace a managed pane's process in place, with an argv command.
  - `rename(kind, target_id, label)`
  - `close(kind, target_id)`
  - `focus(kind, target_id)`
  - `run_command(pane_id, command)`
  - `resize(pane_id, cols, rows)` — Size the pane's WINDOW with tmux, then report what tmux actually did.
  - `_window_geometry(window_id)` — Measured cols/rows of every pane in one window, keyed by pane id.
  - `_client_count(session_id)` — How many clients are attached to the pane's session (unknown -> None).
  - `send_text(pane_id, text)`
  - `send_text_and_enter(pane_id, text)`
  - `send_keys(pane_id, keys)`
  - `read_pane(pane_id, lines, source, ansi)`
- const `DEFAULT_BACKEND_NAME` = `'tmux'`
- `backend_name()` — The backend that would be selected now: the env override, else the default.
- `get_backend(herdr)` — Return the configured terminal backend.

## dunk_tmux_guard.py (303 lines)

> tmux guard - installed by `dunkingsheep guard install-tmux`; do not edit.  The herdr guard's counterpart for the tmux backend. It shadows `tmux` *for the

- const `DUNKINGSHEEP` = `os.environ.get('DUNKINGSHEEP_CLI') or os.path.expanduser('~/.local/bin/dunkingsheep')`
- const `GUARD_DIR` = `os.path.dirname(os.path.realpath(__file__))`
- const `NEWLINE` = `chr(10)`
- const `MARKER` = `b'dunkingsheep-tmux-guard'`
- const `READ_ONLY` = `{'list-panes', 'list-windows', 'list-sessions', 'list-clients', 'list-keys', 'list-command`
- const `DESTRUCTIVE` = `{'kill-pane': 'pane', 'kill-window': 'tab', 'kill-session': 'workspace'}`
- const `TYPING` = `{'send-keys', 'send-text', 'paste-buffer', 'set-buffer', 'load-buffer', 'run-shell', 'if-s`
- const `GLOBAL_WITH_ARG` = `{'-L', '-S', '-f', '-T'}`
- const `GLOBAL_FLAGS` = `{'-2', '-C', '-D', '-l', '-N', '-q', '-u', '-v', '-V'}`
- class **Decision**
  - `__init__(allowed, reason)`
  - `__repr__()`
- `config_dir()`
- `load_protected(path)` — Pane ids the daemon treats as superusers (the VP/owner).
- `load_managed(path)` — Pane ids the tmux backend recorded as created by Dunking Sheep.
- `tmux_panes(globals_)` — Read-only pane inventory for the policy inputs (never a mutation).
- `_target_panes(kind, target, panes)`
- `close_decision(kind, target, panes, protected, managed, caller_pane)` — Decide a kill-pane/kill-window/kill-session without performing it.
- `is_guard(path)`
- `real_tmux()` — The actual tmux binary, never this wrapper.
- `passthrough(argv)`
- `split_global(argv)` — Return (global_options, command, rest). tmux accepts its global flags
- `option_value(rest, name)`
- `current_target(kind, pane_id, globals_)` — kill-window/kill-session with no -t mean the current window/session.
- `own_pane()` — The pane this shell is in, or None. An empty TMUX_PANE is not a pane.
- `route_close(kind, target, globals_)`
- `refuse(command, message)`
- `main(argv)`

## dunk_usage.py (210 lines)

> Configurable real-usage runner (owner #1110).  The usage config maps each provider to a command line, a timeout and a parse

- const `SCHEMA` = `1`
- const `_MISSING` = `object()`
- class **UsageError**(ValueError)
- `_number(value, name)`
- `load_config(path)`
- `_path(value, dotted)`
- `_error(name, message)`
- `run_provider(name, spec, max_age)`
- `run(config, only, max_age)`
- `_windows(reading)`
- `render_text(results)`

## dunk_views.py (435 lines)

> Read-only compositions for the Command Center: roster, message state, project summary.  Three consumers (COS chat, the project page, the agent page) each need a shape that

- const `CONFIG_DIR` = `os.environ.get('DUNKINGSHEEP_DIR', os.path.expanduser('~/.config/dunkingsheep'))`
- const `APP_DB` = `os.environ.get('DUNKINGSHEEP_APP_DB', os.path.expanduser('~/workspace/vp-of-agentry/data/m`
- const `TRANSPORT_STATES` = `('sent', 'read', 'acked')`
- const `REPLY_STATES` = `('posted',)`
- const `MODEL_SOURCES` = `{'claude': ('claude agents --json', 'the session list carries no model field'), 'codex': (`
- `cell(value, as_of, reason)`
- `_iso(at)`
- `roster(flock, now)` — One row per lane: identity, state, current task, inbox, cost, cpu.
- `_cpu_for(flock, leases, now)` — CPU for a lane's caller pid, from /proc, or null with a reason.
- `_app_row(message_id)` — One row from the application's log, or None. Never raises on a missing DB.
- `_app_state(row, now)` — sent / delivered / seen / replied for an APPLICATION message, with evidence.
- `app_messages(channel, limit)` — Recent application messages, newest first: what the owner chat renders.
- `message_state(flock, message_ids, now)` — sent / delivered / seen / replied for owner messages, with timestamps.
- `reply_stats(flock, mailboxes, now)` — Time from a message landing to the lane reading it: last and median.
- `_seen_latencies(inbox, mailbox, limit)`
- `_median(values)`
- const `PROJECT_REPOS` = `{'dunkingsheep': '~/workspace/dunkingsheep', 'vp-of-agentry': '~/workspace/vp-of-agentry',`
- `git_merges(repo, limit, timeout)` — Recent merges for one repository, with its own as_of. Deterministic, no model.
- `project_summary(flock, project, path, now)` — Everything one project page needs, from the store and the DB.

## dunk_watchdog.py (1998 lines)

> Deterministic exception watchdog for the Dunking Sheep fleet.  No model is in this loop, and none may be added: every rule is a fixed

- const `SCHEMA_ID` = `'dunkingsheep-watchdog/2'`
- const `SEVERITIES` = `('info', 'warning', 'critical')`
- const `SEVERITY_RANK` = `{name: rank for rank, name in enumerate(SEVERITIES)}`
- const `CLASSES` = `('security', 'outage', 'money', 'ops', 'project')`
- const `OWNER_CLASSES` = `('security', 'outage', 'money')`
- const `SOURCE_RULE` = `'watchdog-rule'`
- const `SOURCE_SELF` = `'watchdog-self'`
- const `SELF_ERROR_RULE` = `'watchdog.self_error'`
- const `SYSTEM_SENDER` = `'watchdog'`
- class **ConfigError**(Exception)
  - `__init__(key, reason)`
- class **OwnerChannelUnavailable**(Exception)
- `_num(minimum, maximum, integer, positive)`
- const `POSITIVE` = `_num(positive=True)`
- const `POSITIVE_INT` = `_num(integer=True, positive=True)`
- const `FRACTION` = `_num(maximum=1, positive=True)`
- `_bool(value)`
- `_str(value)`
- `_opt_str(value)`
- `_enum(*choices)`
- `_enum_list(*choices, allow_empty)`
- `_str_list(value)`
- `_hex_list(value)`
- `_regex_list(value)`
- `_opt_positive(value)`
- `_regex(value)`
- const `AMBIGUOUS_READER_MARKER` = `'ambiguous composer/transcript'`
- const `RULE_COMMON` = `{'enabled': _bool, 'severity': _enum(*SEVERITIES), 'class': _enum(*CLASSES), 'cooldown_min`
- const `RULE_PARAMS` = `{'fleet.assignment_starvation': {'quiet_minutes': POSITIVE, 'min_empty_workers': POSITIVE_`
- const `TOP` = `{'schema': _enum(SCHEMA_ID), 'enabled': _bool, 'dry_run': _bool, 'cadence_minutes': POSITI`
- `_validate(spec, value, prefix)`
- const `HOLD_ATTEMPT_LIMIT_DEFAULT` = `10`
- const `RULE_DEFAULTS` = `{'severity': 'warning', 'class': 'ops', 'cooldown_minutes': 240, 'must_hold_runs': 1, 'ena`
- const `RULE_ADDITIONS` = `('delivery.ambiguous_queued', 'fleet.assignment_starvation')`
- `migrate_config(config)` — Fill in rules a config predates (DISABLED). Returns the list of added rule ids.
- const `_RULE_EXAMPLE_DEFAULTS` = `{('fleet.assignment_starvation', 'quiet_minutes'): 10, ('fleet.assignment_starvation', 'mi`
- `validate_config(config)` — Raise ConfigError naming the first bad key; return the config unchanged.
- `load_config(path)`
- `_expand(path)`
- `_ro(path)`
- `_load_json(path)`
- class **Inputs**
  - `__init__(config, now, panes_provider, host_sampler)`
  - `_get(name, loader)`
  - `path(name)`
  - `tasks()`
  - `inbox(sql, params)`
  - `starvation_snapshot(since)` — Current global backlog and bounded progress; do not load event history.
  - `metrics(sql, params)`
  - `dunks()`
  - `health()`
  - `panes()` — {pane_id: agent_status} or None when the pane source is disabled.
  - `host(path)`
  - `name_to_pane()` — Mailbox name -> pane id, from the inbox's own routing history (the
- `sample_host(path)`
- `finding(rule_id, rule, key, summary, threshold, measured, window, evidence, source, once)` — One finding. `threshold`, `measured` and `window` are value+unit strings
- `_minutes(seconds)`
- const `HELD_OWNER_MARKER` = `'HELD FOR OWNER DECISION'`
- `_is_held(task)`
- `rule_task_blocked_stale(inputs, rule)`
- `rule_task_lease_stale(inputs, rule)`
- `rule_dunk_skip_loop(inputs, rule)` — A dunk that is still skipping. Not one that skipped for a while and stopped.
- `rule_dunk_overdue(inputs, rule)`
- `_retired_mailbox(inputs, mailbox)` — A retired pane's mail is the record of what was tried, not a condition.
- `_mailbox_groups(rows)`
- `_group_evidence(inputs, mailbox, group, samples)`
- `_retired_panes(inputs)`
- `rule_inbox_undeliverable(inputs, rule)`
- `_queued(inputs, older_than)`
- `rule_inbox_unread_live(inputs, rule)` — Unread mail in a lane that is IDLE: a message it never saw.
- `rule_inbox_unread_dead(inputs, rule)`
- `rule_inbox_ambiguous_sender(inputs, rule)`
- `_pane_activity(inputs, since)` — Pane id -> latest activity time from tasks and inbox (read-only).
- `_holders(inputs, statuses)` — Task id -> sorted holder pane ids, from leases and pane-resolvable assignees.
- `rule_lane_working_without_task(inputs, rule)`
- `rule_lane_idle_holding_task(inputs, rule)`
- `rule_fleet_assignment_starvation(inputs, rule)` — Empty ready queues are a supply problem, even while reviewers are busy.
- `_heavy_jobs(inputs)` — Heavy slot holders plus free-model slots currently running.
- const `_AMBIGUOUS_SHAPE_RE` = `re.compile('^ambiguous composer/transcript: prompt row (\\d+) is separated from footer row`
- `_ambiguous_shape(reason)` — The integers in the documented sentence, or {} when the text is not that sentence.
- `rule_delivery_ambiguous_queued(inputs, rule)` — t409 OPTION 1: a message held on an AMBIGUOUS composer, said out loud, never acted on.
- `rule_host_load(inputs, rule)` — A band fitted to our own recorded work (2026-10-22), not to a guess.
- `rule_host_tmp(inputs, rule)`
- `_slot_holder_alive(slot)` — True if the slot records a live holder pid; None for a legacy slot.
- `rule_pane_retired(inputs, rule)` — Report how many panes are retired, in the info digest.
- `rule_resource_would_kill(inputs, rule)` — Report a lane that would trip a resource policy while it is doing its job.
- const `CARRIED_FAILURE_MAX_AGE_S` = `float(os.environ.get('DUNKINGSHEEP_CARRIED_FAILURE_MAX_S', '43200'))`
- `_recent_failure(failure, now)` — A delivery failure worth carrying: recent, and not already superseded by a success.
- `rule_host_heavy_slot_stale(inputs, rule)` — A poolrad-heavy slot is stale only when its holder is gone.
- class **RuleUnavailable**(Exception)
- `_text_records(inputs, since)` — (kind, id, text) for the content the secret scan covers: inbox bodies,
- `_secret_hits(text, patterns, allow)`
- `rule_security_secret_text(inputs, rule)`
- `rule_money_spend_projection(inputs, rule)`
- `rule_host_disk_full(inputs, rule)`
- `rule_health_failed(inputs, rule)`
- const `RULES` = `{'fleet.assignment_starvation': rule_fleet_assignment_starvation, 'task.blocked_stale': ru`
- const `OWNER_ELIGIBLE_RULES` = `('host.disk_full', 'security.secret_text', 'money.spend_projection')`
- const `PANE_RULES` = `('inbox.unread_dead', 'lane.working_without_task', 'lane.idle_holding_task', 'fleet.assign`
- const `REPLAYABLE_RULES` = `('dunk.skip_loop', 'inbox.ambiguous_sender')`
- `_sort_key(item)`
- `evaluate(config, now, panes_provider, host_sampler, only)` — Run every enabled rule (or only those named). A rule that raises is
- `self_error(problems)` — Exactly one self finding per run, carrying every raw problem.
- `empty_state()`
- `load_state(path)` — (state, problem). A missing or corrupt file gives a fresh state AND a
- `save_state(path, state)`
- `dedupe_key(item)`
- `advance_streaks(config, state, findings, now)` — Consecutive-run counts per finding, as they would be after this run. A
- `_cooling(config, state, item, now)`
- `plan(config, findings, state, streaks, now, owner_blocked)` — Decide, per finding, VP notify and owner page with the reason for any
- `recoveries(state, findings)` — Keys alerted before and absent now: VP recovery lines, plus one owner
- `_evidence(item)`
- `line(prefix, item, suppressed, streak, hold)`
- `render_lines(config, decisions, skipped, recovered, mode)`
- `digest(title, lines)`
- `inbox_sender(to, subject, body)` — VP delivery through the daemon's inbox API as the `watchdog` system
- `send_starvation_group(config, state, item, now, send)` — Validated superuser destinations; remember each accepted send separately.
- class **DeskBriefChannel**
  - `__init__(confirm_path)`
  - `desk_channel()`
  - `post(body)`
  - `confirmed_through()`
- `_confirm_pending(config, state, owner_channel, now, problems)` — Mark pending owner pages confirmed when the forwarder has passed them;
- `run(config, now, dry_run_flag, send, owner_channel, panes_provider, host_sampler)` — One watchdog run. Live only when config enabled=true AND dry_run=false AND
- `write_heartbeat(path, now, cadence_minutes)` — The heartbeat records the effective cadence, so the dead-man can widen
- `render_text(report)`
- `replay(config, start, end, step_minutes, panes_provider, host_sampler)` — Re-evaluate the history-backed rules at each step from `start` to `end`

## dunk_watchdog_deadman.py (293 lines)

> Dead-man check for the exception watchdog (quis custodiet).  Deliberately independent of `dunk_watchdog`: it does not import it, read its

- const `RULE_ID` = `'watchdog.deadman'`
- const `SYSTEM_SENDER` = `'watchdog'`
- const `CLAMP_FACTOR` = `6`
- const `CAUSES` = `('daemon_down', 'config_bad', 'other')`
- `daemon_path_problem(daemon_path, expected_dir)` — A misconfigured daemon path raises the same FileNotFoundError as a dead
- `classify_daemon_error(error, daemon_path, expected_dir)` — daemon_down | config_bad | other. Only daemon_down may write directly.
- `heartbeat_age(path, now)` — Seconds since the heartbeat, or None if it does not exist.
- `recorded_cadence(path)` — (cadence_minutes or None, problem or None) from the heartbeat body.
- `effective_cadence(unit, recorded)` — max(unit, recorded), clamped to CLAMP_FACTOR x unit. (minutes, clamped).
- `_load(path)`
- `_save(path, data)`
- `_read_error(path)`
- `daemon_sender(daemon_path)`
- `direct_write(inbox_path, target, subject, text)` — The fallback: one row straight into the inbox store, read back to verify.
- `check(heartbeat, cadence_minutes, multiplier, to, state_path, cooldown_minutes, now, send, dry_run, error_file, status_path, inbox_path, daemon_path, expected_dir)` — Return a report; send at most one alert. `send(to, subject, body)`.
- `main(args)`

## dunkingsheep (3229 lines)

> dunkingsheep - command-line control of Dunking Sheep.      dunkingsheep add --target "Codex hunterdavis.com" --every 15 --text continue

- const `HERE` = `os.path.dirname(os.path.abspath(__file__))`
- const `CLI_ABS` = `os.path.abspath(__file__)`
- `build_parser()`
- const `HARNESSES` = `('claude', 'codex', 'gemini', 'cursor', 'json', 'toml')`
- `mcp_setup_text(harness, scope)` — Return (text, apply_command or None) for one harness.
- `run_mcp_setup(args)`
- `_clip(text, width)`
- `_every(minutes)`
- `_gate(dunk)`
- `print_dunks(dunks)`
- `print_dunk(dunk)`
- `print_panes(result)`
- `print_inbox_archive(result)`
- `print_load(r)`
- `print_pm_digest(r)`
- `print_pm_routing(r)`
- `print_inbox_count(r)`
- `print_inbox_sent(r)`
- `_inbox_rows(messages)`
- `print_inbox_list(r)`
- `_task_row(task)`
- `print_task(task)`
- `print_task_list(result)`
- `print_task_next(result)`
- `print_task_followers(result)`
- `print_task_stats(result)`
- `print_memory(rows)`
- `print_memory_context(result)`
- `print_memory_hook_context(result)` — Claude SessionStart hook output carrying the memory digest.
- `print_memory_backfill(result)`
- `print_memory_stats(result)`
- `print_health(result)`
- `print_doctor(result)`
- `print_recover(result)`
- `_context_body(args)`
- `_context_via_daemon(args)` — context add/promote/supersede/retire over the socket, role-gated there.
- `_git_credential(client, operation)` — git credential protocol on stdin/stdout: only `get` for https github.com,
- `_github_local(args)` — Owner's one-shot setup with their own `gh` login (never a lane path).
- const `GH_WRAPPER` = `'#!/bin/sh\n# dunkingsheep gh wrapper: lanes act as their GitHub App; the owner keeps thei`
- `dunk_github_wrappers_install(bin_dir)`
- `print_poolrad_sync(result)`
- `print_poolrad_progress(result)`
- `print_review_sync(result)`
- `print_review_list(result)`
- `print_whoami(result)`
- `print_superuser(result)`
- `print_task_metrics(result)`
- `print_inbox_read(r)`
- `_print_inbox_tasks(r)`
- `print_inbox_recover(r)`
- `print_inbox_status(r)`
- `print_inbox_stats(r)`
- `print_message(result)`
- `print_capability(result)`
- `print_lanes(result)`
- `print_lane(result)`
- `print_retired(result)`
- `print_owner_push(result)`
- `print_owner_take(result)`
- `print_owner_stats(result)`
- `print_view(result)` — The Command Center views on a terminal: JSON, because a UI consumes them.
- `print_dispatch(result)` — One line per number. A tick says what it did; a status says what it knows.
- `print_clear(result)` — A clear is not a message: it reports the state it found and the state it left.
- `print_messages(result)`
- `print_workspaces(result)`
- `print_created(result)`
- `print_herdr(result)`
- `print_poll(result)`
- `print_status(result)` — Status, with the code's identity beside the constant.
- `mcp_map_text(markdown)` — Render the t8 OLD -> NEW tool map (text or markdown).
- `_be_nice()` — A poller must not outrank an interactive agent. Courtesy, not a fix.
- const `LOCAL_ONLY_COMMANDS` = `('context', 'github')`
- const `CONTEXT_DAEMON_WRITES` = `('add', 'promote', 'supersede', 'retire')`
- const `SEND_TIMEOUT_S` = `float(os.environ.get('DUNKINGSHEEP_SEND_TIMEOUT_S', '120'))`
- const `BATCH_TIMEOUT_S` = `float(os.environ.get('DUNKINGSHEEP_BATCH_TIMEOUT_S', '180'))`
- const `EXIT_STILL_WORKING` = `75`
- const `BATCH_COMMANDS` = `('dispatch',)`
- `_still_working_message(command, seconds)`
- const `_MUTATING_COMMANDS` = `('send', 'send-keys', 'fire', 'flush', 'clear-box', 'start', 'stop', 'toggle', 'stop-all',`
- const `_READ_ONLY_SUBCOMMANDS` = `{'task': {'list', 'ls', 'read', 'stats', 'metrics', 'followers', 'get'}, 'inbox': {'count'`
- `_is_mutating(words)` — True when this invocation can change state, and so invalidates cached reads.
- `_generation_path()`
- `_generation()`
- `_bump_generation(phase)` — Invalidate cached reads. Called BEFORE a mutating run and AGAIN after it.
- const `_POLL_COMMANDS` = `('status', 'list', 'ls', 'poll')`
- const `_INBOX_POLLS` = `('count', 'list', 'ls', 'stats')`
- `_coalesce(argv)` — Return (cache path, hit) for a poll command; path is None when uncacheable.
- `_run(argv)` — Run main(), caching the stdout of a coalescable command.
- `main(argv)`
- `run_context_command(dunk_context, args)` — The context store commands. Local files only: no socket, no daemon.
- const `SHELL_SUBSTITUTION` = `('`', '$(', '${')`
- `_check_send_body(args)` — Guard the send path before anything else happens.
- `_respawn_argv(args)` — Parse and validate respawn argv in one place.
- class **Refused**(SystemExit)
  - `__init__(message, code)`
  - `__str__()`
- `_refuse_shell_substitution(body, subject, literal)` — Refuse a body the sending shell was free to rewrite.
- `print_turn_end(result)` — Say what the turn end DID, not only that it was recorded.
- `run_client_command(client, args)` — Map CLI args onto a registry command; return (result, human printer).

## herdr_client.py (490 lines)

> Thin wrapper around the `herdr` CLI, which speaks to the running herdr server over its unix socket and returns JSON. Dunking Sheep uses this the same way Dunking Bird used ydotool/kdotool: shell out to a trusted local binary.

- const `SEND_TEXT_CHUNK_CHARS` = `900`
- const `SEND_TEXT_CHUNK_DELAY_S` = `0.05`
- const `SEND_TEXT_SETTLE_DELAY_S` = `0.2`
- const `AGENT_KINDS` = `frozenset({'pi', 'claude', 'codex', 'gemini', 'cursor', 'devin', 'agy', 'cline', 'omp', 'm`
- `_text_chunks(text, max_chars)` — Yield non-empty character chunks without splitting a CRLF pair.
- const `GUARD_DIR` = `os.path.join(os.environ.get('DUNKINGSHEEP_DIR') or os.path.expanduser('~/.config/dunkingsh`
- const `GUARD_MARKER` = `b'dunkingsheep-herdr-guard'`
- `_is_guard(path)` — True if `path` is the Dunking Sheep herdr wrapper rather than herdr.
- `_find_herdr()` — Locate the *real* herdr binary, preferring PATH then the usual install
- class **HerdrClient**
  - `__init__(binary, default_timeout)`
  - `_run(args, timeout)` — Run `herdr <args>`; return (returncode, stdout, stderr).
  - `_run_json(args, timeout)` — Run a command whose stdout is a single JSON object; return the dict
  - `_run_result(args, timeout)` — Run a JSON-printing herdr command; return (True, result) or
  - `version()` — The herdr CLI version string (e.g. 'herdr 0.9.5'), or None.
  - `is_available()` — True if the herdr binary exists and the server is reachable.
  - `server_error_hint()` — Human-readable reason the server looks unavailable.
  - `list_panes()` — Return a list of pane dicts (possibly empty).
  - `get_pane(pane_id)` — Return a single pane dict, or None if it no longer exists.
  - `list_tabs()` — Return a list of tab dicts (possibly empty).
  - `tab_labels()` — Map of tab_id -> human label for annotating panes.
  - `list_panes_with_tabs()` — Panes from `list_panes`, each annotated with a `tab_label` key.
  - `list_workspaces()` — Return a list of workspace dicts (possibly empty).
  - `list_panes_grouped()` — Panes annotated with tab + workspace labels/numbers and sorted by
  - `_placement(cwd, label, focus, env)`
  - `create_workspace(label, cwd, focus, env)`
  - `create_tab(workspace_id, label, cwd, focus, env)`
  - `split_pane(pane_id, direction, cwd, ratio, focus, env)`
  - `start_agent(name, argv, workspace_id, tab_id, split, cwd, focus, env)` — Start agent `name` in a new shell pane.
  - `rename(kind, target_id, label)`
  - `close(kind, target_id)`
  - `focus(kind, target_id)`
  - `run_command(pane_id, command)` — Type a shell command + Enter into a pane (`herdr pane run`).
  - `send_text_and_enter(pane_id, text)` — Send literal text to a pane, then press Enter. Returns (ok, msg).
  - `send_keys(pane_id, keys)` — Press one or more named keys in a pane (`herdr pane send-keys`).
  - `send_text(pane_id, text)` — Send literal text to a pane without pressing Enter. Returns (ok, msg).
  - `agent_status(pane_id)` — Return the pane's agent_status (idle|working|blocked|unknown), or
  - `read_pane(pane_id, lines, source, ansi)` — Return the last `lines` lines of a pane's output as text, or None if
  - `agent_child_pids(pane_pid)` — Kernel process entries for the agent behind `pane_pid`.
  - `self_pane_id()` — The pane this process runs in, if herdr launched it.
  - `notify(title, body)` — Fire-and-forget herdr toast notification.

## dunking_sheep_tui.py (783 lines)

> Dunking Sheep TUI - terminal interface for automated text sending.  This is a terminal-native sibling of Dunking Bird. Each "dunker" targets a terminal

- const `HERE` = `os.path.dirname(os.path.abspath(__file__))`
- const `REFRESH_MS` = `200`
- const `RECONNECT_S` = `2.0`
- `clip(value, width)`
- class **LocalFlock**
  - `__init__()`
  - `call(cmd)`
  - `close()`
- class **DunkingSheepTui**
  - `__init__(stdscr)`
  - `connect()`
  - `rpc(cmd)` — Run a command against the daemon; errors become a status flash.
  - `refresh()`
  - `refresh_tasks()`
  - `run()`
  - `flash(text, seconds)`
  - `current()`
  - `current_id()`
  - `handle_key(key)`
  - `current_task()`
  - `handle_task_key(key)` — Keys for the tasks view. All coordination goes through the daemon.
  - `quit_and_shutdown()`
  - `edit_interval()`
  - `edit_text()`
  - `edit_name()`
  - `toggle_only_idle()`
  - `toggle_skip_if_unconsumed()`
  - `toggle_hold_while_typing()`
  - `edit_max_sends()`
  - `pick_target()` — Choose a terminal pane to target (replaces window capture).
  - `line_modal(label, default)`
  - `target_modal(panes)` — Full-screen picker of terminal panes, grouped by workspace with the tab
  - `_format_target_row(tab, agent, status, cwd, width, indent)`
  - `text_modal(initial_text)`
  - `draw()`
  - `draw_tasks()`
  - `format_task_row(num, status, priority, scope, assignees, title, width)`
  - `format_row(num, status, window, minutes, sent, gate, text, width)`
  - `safe_hline(win, y, x, width)`
  - `safe_addstr(win, y, x, value, width, attr)`
  - `shutdown()`
- `main()`


## Web routes (web/app.py as of PR #120, for T62)

- `post /api/login` (line 320)
- `post /api/logout` (line 342)
- `post /api/password` (line 357)
- `post /api/viewport` (line 419)
- `get /api/channels` (line 450)
- `get /api/pane-text` (line 472)
- `get /api/capability` (line 558)
- `post /api/pane-send` (line 575)
- `get /api/fleet` (line 697)
- `get /api/ops-metrics` (line 832)
- `get /api/ops-metrics/history` (line 846)
- `post /api/ops-metrics/refresh` (line 863)
- `get /api/ops-board` (line 877)
- `post /api/ops-board/refresh` (line 893)
- `get /api/game/status` (line 1260)
- `get /api/game/codewheel/{filename}` (line 1265)
- `post /api/game/start` (line 1278)
- `post /api/game/stop` (line 1286)
- `post /api/game/key` (line 1291)
- `post /api/game/type` (line 1300)
- `get /api/game/states` (line 1309)
- `post /api/game/savestate` (line 1314)
- `post /api/game/loadstate` (line 1322)
- `post /api/game/autocodeword` (line 1330)
- `get /api/game/video` (line 1342)
- `get /api/game/audio` (line 1350)
- `get /api/messages` (line 1358)
- `get /api/deliveries` (line 1378)
- `get /api/messages/{message_id}/context` (line 1398)
- `post /api/messages` (line 1426)
- `post /api/messages/{message_id}/send-now` (line 1490)
- `post /api/brief` (line 1553)
- `get /api/search` (line 1580)
- `post /api/uploads` (line 1594)
- `get /api/uploads/{attachment_id}` (line 1611)
- `get /api/panes/activity` (line 1631)
- `get /api/activity` (line 1666)
- `get /api/poolrad-dashboard/progress` (line 1747)
- `post /api/poolrad-dashboard/refresh` (line 1781)
- `get /api/poolrad-dashboard/dashboard.html` (line 1809)
- `get /api/poolrad-dashboard/state/reports/{filename}` (line 1820)
- `get /api/herdr-webui` (line 1857)
- `get /api/health` (line 1871)
- `get /api/version` (line 1876)
- `get /dash` (line 1929)
- `get /dash/` (line 1930)
- `get /dash/{path:path}` (line 1935)
