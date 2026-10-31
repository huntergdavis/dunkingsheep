# Port manifest (generated from 70876c2, 2026-10-30)

Everything the Rust port must reproduce, generated from the tree. Regenerate with `tools/gen_manifest.py` (TASKS.md T00) when main moves; `--check` fails when this document no longer matches the tree it names.

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
| dunkingsheep (CLI) | ds-cli | ds-cli | 1 | generated parser + ported printers; local-only commands (context reads, github one-shots). Parity (t1105): `build_parser` / `run_client_command` have no Rust counterpart until the ds-cli crate exists (the parser is generated there); the `task complete --unmerged` flag (dest=landed, t1044) is carried by the daemon-side `landed` parameter already ported in tasks/mod.rs (t1104, t1106) |
| herdr_client.py | (dropped) | - | - | the herdr backend is retired (tmux default since 2026-10-24); keep the Terminal trait so a backend can return |
| dunking_sheep_tui.py, text_editor.py | ds-tui (ratatui) | ds-tui | 3 | |
| web/, bridges/, ui/ | ds-web (axum) + unchanged React | ds-web | 3 | FastAPI routes 1:1; the owner mailbox/report land here |

## CLI top-level commands

`allocation` `status` `turn` `poll` `list` `get` `add` `update` `remove` `start` `stop` `toggle` `stop-all` `fire` `send` `send-keys` `install` `guard` `inbox` `context` `load` `pm` `task` `gh-token` `git-credential` `review` `poolrad` `github` `whoami` `token` `superuser` `memory` `health` `watchdog` `watchdog-deadman` `usage` `recover` `doctor` `owner` `state` `roster` `message-state` `reply-stats` `project-summary` `api` `capability` `lane` `retired` `dispatch` `clear-box` `delivery` `flush` `messages` `cancel-message` `metrics` `panes` `workspaces` `new-workspace` `new-tab` `split` `start-agent` `respawn-pane` `rename` `focus` `close` `herdr` `herdr-help` `read` `shutdown` `serve` `tui` `mcp` `mcp-setup` `guide` `commands` `mcp-map`

## Files on disk (~/.config/dunkingsheep)

agent-state.db, context-ledger.db, desk.json, dialog-alerts.db, dispatch-state.json, dispatch.db, dispatch_lanes.json, dunks.json, github-app-author.json, github-app-reviewer.json, github-repos.json, github-tokens.json, health.json, inbox.db, lanes.json, launches.json, load-state.json, memory.db, metrics.db, owner-push.db, pm-routing.json, retired-panes.json, roles.json, service-tokens.json, superusers.json, tasks.db, tmux-panes.json, tmux-trial-panes.json, usage.json, watchdog.json, watchdog_deadman_state.json, watchdog_deadman_status.json, watchdog_state.json; plus `context/` (markdown store), `attachments/` (inbox blobs), `bin/` (wrappers), `system-token`, `web-token`, `github-app-*.pem`, `dunkingsheep.sock`, `daemon.log`.

## Systemd units (packaging/systemd)

dunkingsheep, dunkingsheep-deepcheck, dunkingsheep-dispatch, dunkingsheep-health, dunkingsheep-poolrad, dunkingsheep-report, dunkingsheep-review, dunkingsheep-state, dunkingsheep-telegram, dunkingsheep-watchdog-deadman, dunkingsheep-watchdog, dunkingsheep-web (each a .service, most with a .timer).

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
| `owner_conversation` | - | limit, before | y |  | owner, system, web, desk |
| `owner_delivered` | message_id, transport | message_id, transport, external_id | y |  | owner, system, web |
| `owner_ingest` | body, transport | body, transport, external_id, subject, attach | y |  | owner, system, web |
| `owner_outbox` | transport | transport, limit | y |  | owner, system, web, desk |
| `owner_poll` | - | pane_id, by | y |  | owner, system, web, desk |
| `owner_push` | target, body | target, body, subject | y |  | owner, system, web |
| `owner_receipts` | - | push_ids | y |  | owner, system, web, desk, planner, worker, reviewer, anonymous |
| `owner_reply` | message_id, body | message_id, body | y |  | owner, system, web, desk |
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
| `report_list` | - | out_dir | y |  | owner, system, web, desk, planner, worker, reviewer, anonymous |
| `report_run` | - | post, prose, out_dir | y |  | owner, system, web, desk |
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
| `turn_end` | - | pane, turn_id, reason | n |  | owner, system, web, desk, planner, worker, reviewer, anonymous |
| `turn_start` | - | pane, ttl_s, source, handoff_id | n |  | owner, system, web, desk, planner, worker, reviewer, anonymous |
| `update_dunk` | id | id, target, text, interval_minutes, name, only_when, max_sends, skip_if_unconsumed, hold_while_typing | y | y | owner |
| `whoami` | - | - | y |  | owner, system, web, desk, planner, worker, reviewer, anonymous |

## MCP tool groups

- **inbox**: send->inbox_send, count->inbox_count, list->inbox_list, read->inbox_read, recover->inbox_recover, status->inbox_status, ack->inbox_ack, cancel->inbox_cancel, delete->inbox_delete, stats->inbox_stats, fetch->inbox_fetch, archive->inbox_archive, conversation->owner_conversation, reply->owner_reply, ingest->owner_ingest, outbox->owner_outbox, delivered->owner_delivered
- **task**: add->task_add, list->task_list, read->task_read, update->task_update, assign->task_assign, complete->task_complete, block->task_block, unblock->task_unblock, cancel->task_cancel, delete->task_delete, note->task_note, next->task_next, heartbeat->task_heartbeat, release->task_release, follow->task_follow, unfollow->task_unfollow, followers->task_followers, stats->task_stats, metrics->task_metrics, reap->task_reap
- **dunk**: list->list_dunks, get->get_dunk, add->add_dunk, update->update_dunk, remove->remove_dunk, start->start_dunk, stop->stop_dunk, stop_all->stop_all, fire->fire_dunk
- **views**: push->owner_push, take->owner_take, poll->owner_poll, stats->owner_stats, roster->roster, message_state->message_state, reply_stats->reply_stats, project_summary->project_summary, capture->state_capture, latest->state_latest
- **dispatch**: dispatch->dispatch, retire->retire_pane, unretire->retire_pane, retired->retire_pane, lanes->lanes, lane_set->lane_set, review_sync->review_sync, review_list->review_list, gh_token->gh_token, poolrad_sync->poolrad_sync, poolrad_progress->poolrad_progress, context_add->context_add, context_promote->context_promote, context_supersede->context_supersede, context_retire->context_retire
- **pane**: list->list_panes, read->read_pane, delivery->delivery, capability->delivery_capability, send->send_text, send_keys->send_keys, flush->flush_box, clear_box->clear_box, messages->list_messages, get_message->get_message, cancel_message->cancel_message
- **workspace**: list->list_workspaces, create->create_workspace, create_tab->create_tab, split->split_pane, start_agent->start_agent, respawn_pane->respawn_pane, rename->rename, focus->focus, close->close
- **superuser**: whoami->whoami, register->lane_register, unregister->lane_unregister, token->token_issue, revoke->token_revoke, status->superuser_status, add->superuser_add, remove->superuser_remove, recover->superuser_recover
- **memory**: recall->memory_recall, context->memory_context, search->memory_search, stats->memory_stats, status->memory_status, ingest->memory_ingest, backfill->memory_backfill
- **metrics**: summary->metrics, status->status, health->health, report->report_run, reports->report_list
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

## Schemas (snapshot of the live databases, verbatim; rust/fixtures/state/schema.json - refresh with tools/snapshot_state_schema.py --write)


## tasks.db
CREATE TABLE allocation_offers (
 task_id TEXT PRIMARY KEY, pool TEXT NOT NULL, role TEXT NOT NULL,
 requires TEXT NOT NULL, excluded TEXT NOT NULL, owners TEXT NOT NULL,
 offered_by TEXT NOT NULL, state TEXT NOT NULL DEFAULT 'ready');
CREATE TABLE allocation_reviews (
 source_id TEXT PRIMARY KEY, task_id TEXT NOT NULL UNIQUE,
 revision TEXT NOT NULL, author TEXT NOT NULL);
CREATE TABLE allocation_workers (
 pane_id TEXT PRIMARY KEY, pools TEXT NOT NULL, roles TEXT NOT NULL,
 skills TEXT NOT NULL, enabled INTEGER NOT NULL, registered_by TEXT NOT NULL,
 updated_at REAL NOT NULL, last_assigned REAL NOT NULL DEFAULT 0);
CREATE TABLE counters (name TEXT PRIMARY KEY, value INTEGER NOT NULL);
CREATE INDEX leases_by_pane ON task_leases (pane_id);
CREATE TABLE sqlite_sequence(name,seq);
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
CREATE INDEX task_events_by_event ON task_events (event, at);
CREATE INDEX task_events_by_task ON task_events (task_id, at);
CREATE INDEX task_events_by_time ON task_events (at);
CREATE TABLE task_leases (
    task_id     TEXT NOT NULL,
    pane_id     TEXT NOT NULL,
    claimed_at  REAL NOT NULL,
    heartbeat_at REAL NOT NULL,
    expires_at  REAL NOT NULL, caller_pid INTEGER, caller_start TEXT, caller_name TEXT, assignee_added INTEGER NOT NULL DEFAULT 0,
    PRIMARY KEY (task_id, pane_id)
);
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
, kind TEXT NOT NULL DEFAULT 'work', author TEXT, repo TEXT, pr INTEGER, head TEXT, verdict TEXT, accounting_pr INTEGER);
CREATE INDEX tasks_by_parent ON tasks (parent_id);
CREATE INDEX tasks_by_scope
    ON tasks (scope, status, created_at);
CREATE UNIQUE INDEX tasks_one_review_per_head ON tasks (kind, repo, pr, head) WHERE kind = 'review' AND head IS NOT NULL AND status IN ('open', 'in_progress', 'blocked');

## inbox.db
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
CREATE TABLE deliveries (
    mailbox       TEXT PRIMARY KEY,
    message_ids   TEXT NOT NULL,
    delivered_at  REAL NOT NULL
);
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
, archived_at REAL, digest_id TEXT, digest_updated_at REAL, notify_at REAL, notify_transport_id TEXT, notify_attempt INTEGER, transport TEXT, reply_to TEXT, external_id TEXT, delivered_at REAL, delivery_transport TEXT);
CREATE UNIQUE INDEX messages_by_digest ON messages (digest_id) WHERE digest_id IS NOT NULL;
CREATE UNIQUE INDEX messages_by_external ON messages (mailbox, external_id) WHERE external_id IS NOT NULL;
CREATE INDEX messages_by_mailbox
    ON messages (mailbox, state, created_at);
CREATE INDEX messages_by_state
    ON messages (state, pane_id);
CREATE INDEX messages_by_transport ON messages (mailbox, transport, created_at);

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
CREATE INDEX events_by_kind ON events (kind, at);
CREATE INDEX events_by_time ON events (at);
CREATE TABLE sqlite_sequence(name,seq);

## dispatch.db
CREATE TABLE sqlite_sequence(name,seq);
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
CREATE TABLE owner_poll (
    pane_id   TEXT NOT NULL,
    at        REAL NOT NULL,
    by        TEXT,
    PRIMARY KEY (pane_id)
);
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
CREATE INDEX owner_push_pane ON owner_push(pane_id, taken_at);
CREATE TABLE sqlite_sequence(name,seq);

## memory.db
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
CREATE TRIGGER chunks_ad AFTER DELETE ON chunks BEGIN
    INSERT INTO chunks_fts(chunks_fts, rowid, text)
        VALUES ('delete', old.id, old.text);
END;
CREATE TRIGGER chunks_ai AFTER INSERT ON chunks BEGIN
    INSERT INTO chunks_fts(rowid, text) VALUES (new.id, new.text);
END;
CREATE TRIGGER chunks_au AFTER UPDATE ON chunks BEGIN
    INSERT INTO chunks_fts(chunks_fts, rowid, text)
        VALUES ('delete', old.id, old.text);
    INSERT INTO chunks_fts(rowid, text) VALUES (new.id, new.text);
END;
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
CREATE TABLE 'chunks_fts_config'(k PRIMARY KEY, v) WITHOUT ROWID;
CREATE TABLE 'chunks_fts_data'(id INTEGER PRIMARY KEY, block BLOB);
CREATE TABLE 'chunks_fts_docsize'(id INTEGER PRIMARY KEY, sz BLOB);
CREATE TABLE 'chunks_fts_idx'(segid, term, pgno, PRIMARY KEY(segid, term)) WITHOUT ROWID;
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
CREATE TABLE schema_meta (
    key   TEXT PRIMARY KEY,
    value TEXT NOT NULL
);
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
CREATE INDEX sessions_by_harness ON sessions(harness, updated_at);
CREATE INDEX sessions_by_project ON sessions(project, updated_at);
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

## Tests (150 files; port each 1:1 under rust/crates/*/tests or as unit tests in the module)

_bootstrap fakes fixtures test_allowlist_doc test_backpressure_metric_reason test_busy_native_queue test_check_public_secrets test_claude_turn_generation test_cli test_cli_guard test_cli_json_position
test_cli_read_after_write test_cli_respawn_dispatch test_codex_ambiguous_composer test_codex_turn_generation test_command_smoke test_composer_boundary test_deja_free test_diff_harness
test_dispatch_feeds_reviews test_dunk_activity test_dunk_agent_launch test_dunk_allocator test_dunk_backend_default test_dunk_bootstrap_sweep test_dunk_callers test_dunk_cancelled_handoff
test_dunk_carried_failure test_dunk_client_busy test_dunk_coalesce_freshness test_dunk_codex_busy_frame test_dunk_codex_composer test_dunk_codex_drafts_vs_echo test_dunk_codex_idle_footer
test_dunk_complete_ownership test_dunk_complete_ownership_probe test_dunk_context test_dunk_context_governance test_dunk_core test_dunk_delivery test_dunk_delivery_backoff test_dunk_dialog_alerts
test_dunk_dispatch test_dunk_dispatch_allowlist test_dunk_fallback test_dunk_federated_recovery test_dunk_gate5_message test_dunk_github test_dunk_guard test_dunk_guarded_nudge_submits
test_dunk_health test_dunk_heavy_slot test_dunk_http_resize test_dunk_http_terminals test_dunk_identity test_dunk_identity_tty test_dunk_inbox test_dunk_integration_t514 test_dunk_lane_auto_declare
test_dunk_lane_status test_dunk_list_panes_provenance test_dunk_load test_dunk_mcp test_dunk_memory test_dunk_metrics test_dunk_native_frame test_dunk_native_session test_dunk_native_typed_frame
test_dunk_next_job_handoff test_dunk_next_job_plan test_dunk_next_job_wiring test_dunk_no_self_review test_dunk_outbox test_dunk_outbox_restart test_dunk_owner_fds test_dunk_owner_receipts
test_dunk_pane_pid test_dunk_pane_registry_gap test_dunk_panes test_dunk_pm test_dunk_policy test_dunk_poll test_dunk_poll_floor test_dunk_poolrad test_dunk_process_group test_dunk_read_order
test_dunk_receipts_regression test_dunk_recovery test_dunk_recovery_wiring test_dunk_report test_dunk_require_server test_dunk_respawn_pane test_dunk_response_loss test_dunk_roles
test_dunk_safe_queued_submit test_dunk_scheduled_recovery test_dunk_secrets test_dunk_server test_dunk_shell_bypass test_dunk_sqlite test_dunk_start_agent_verify test_dunk_state_capture_cost
test_dunk_state_session test_dunk_stuck test_dunk_stuck_recovery test_dunk_styled_state test_dunk_styled_wiring test_dunk_submit_honesty test_dunk_submit_when_box_unknown test_dunk_suite_hermetic
test_dunk_superusers test_dunk_task_model test_dunk_task_pool test_dunk_tasks test_dunk_tasks_threads test_dunk_test_leaks test_dunk_tmux_guard test_dunk_turn_confirmation test_dunk_turn_end_dispatch
test_dunk_views test_dunk_watchdog test_dunk_watchdog_starvation test_federated_backoff test_gen_manifest test_herdr_client test_mcp_stdout_binary test_mechanism_census test_memory_parity
test_model_command_suffix test_opencode_turn_generation test_owner_mailbox test_owner_transport test_package_identity test_pi_poll_backoff test_pi_turn_generation test_recover
test_retired_mailbox_archive test_t374_acceptance test_telegram_acceptance_probe test_telegram_page_watermark test_telegram_transport test_terminal test_text_editor test_tmux_guard_resolution
test_turn_auth_diagnostics test_turn_producer_installers test_unit_packaging test_usage test_verdict_evidence test_watchdog_ambiguous_queued test_web_socket_only

## Inventory: every module, class, function, constant

## dunk_activity.py (121 lines)

> What is an agent DOING - from the tool that knows, not from the screen.  Screen scraping answered this for a year and lied in both directions: a working pi

- const `STATES` = `("working", "waiting", "idle", "unknown")`
- const `IDLE_CLAIM_S` = `float(os.environ.get("DUNKINGSHEEP_IDLE_CLAIM_S", str(6 * 3600)))`
- `turn_state(state, now, idle_claim_s, harness)` — THE one rule for a declared turn.
- `activity(pane, claude_status, turns, now)` — The best available answer about one lane's activity, with its source.
- `_claude(pane, claude_status, now)`
- `_pi(pane, turns, now)`
- `say(record)` — One line for a log or a table.
- `now_hours(at, now)`

## dunk_allocator.py (299 lines)

> Registered, deterministic allocation. Task DB is the ownership authority.  Registration and offers are explicit contracts, never inferred from task prose.

- const `SCHEMA` = `""" CREATE TABLE IF NOT EXISTS allocation_workers ( pane_id TEXT PRIMARY KEY, pools TEXT N`
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

## dunk_api.py (2983 lines)

> The Dunking Sheep command registry.  One table of named commands drives all three remote control surfaces:

- const `DEFAULT_SEND_WAIT_S` = `3.0`
- const `TARGET_DOC` = `"Which terminal pane. Accepts a pane id like '%N' (tmux) or 'w8:p3' (herdr rollback), the `
- const `TEXT_DOC` = `"Text to type into the pane, followed by Enter. Placeholders {id}, " "{name}, {count}, {ta`
- class **Command**
  - `__init__(name, description, handler, params, required, mcp, mcp_required)`
  - `input_schema()`
- `_opt(args, key, default)`
- `_bool(value, default)`
- const `TASK_ID_PARAM` = `{"type": "string", "description": "Task id, e.g. t12."}`
- const `TASK_SCOPE_PARAM` = `{ "type": "string", "description": "'global' for the fleet queue, a pane id for a private `
- const `TASK_SCOPE_ADD_PARAM` = `{ "type": "string", "description": "'global' (default, fleet-wide), 'self' (private to the`
- const `TASK_SCOPE_NEXT_PARAM` = `{ "type": "string", "description": "'global' (default), 'self' (own pane queue), 'auto' (p`
- const `TASK_TITLE_PARAM` = `{"type": "string", "description": "Short task title."}`
- const `TASK_BODY_PARAM` = `{ "type": "string", "description": "Longer description or acceptance notes.", }`
- const `TASK_STATUS_PARAM` = `{ "type": "string", "enum": list(TASK_STATUSES), "description": "open, in_progress, blocke`
- const `TASK_PRIORITY_PARAM` = `{ "type": "string", "enum": list(TASK_PRIORITIES), "description": "urgent, high, medium or`
- const `TASK_ASSIGNEES_PARAM` = `{ "type": "array", "items": {"type": "string"}, "description": "Pane ids or agent names. '`
- const `TASK_PARENT_PARAM` = `{"type": "string", "description": "Parent task id, for a subtask."}`
- const `TASK_DEPS_PARAM` = `{ "type": "array", "items": {"type": "string"}, "description": "Task ids that must be done`
- const `TASK_LEASE_PARAM` = `{ "type": "number", "minimum": 1, "description": "Lease length in seconds (default 1800)."`
- const `SUPERUSER_ENTRY_PARAM` = `{ "type": "string", "description": "A pane id (e.g. w9:p8) or a name matching a pane's " "`
- const `SUPERUSER_NAME_PARAM` = `{ "type": "string", "description": "Name to re-assert (default 'VP of agentry').", }`
- const `MEMORY_QUERY_PARAM` = `{"type": "string", "description": "Memory search text."}`
- const `MEMORY_LIMIT_PARAM` = `{"type": "integer", "minimum": 1, "maximum": 50, "description": "Maximum results (default `
- const `MEMORY_HARNESS_PARAM` = `{"type": "string", "enum": ["claude", "codex", "opencode", "pi", "generic"], "description"`
- const `MEMORY_PROJECT_PARAM` = `{"type": "string", "description": "Only this project label."}`
- const `MEMORY_SINCE_PARAM` = `{"type": "number", "description": "Only chunks updated at/after this unix timestamp."}`
- const `MEMORY_PATH_PARAM` = `{"type": "string", "description": "Session JSONL file or sqlite DB to ingest."}`
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
- const `DISPATCH_LANES_PATH` = `os.path.join(_CONFIG_DIR, "dispatch_lanes.json")`
- `_dispatch_allowlist(flock, args, lanes)` — Which lanes the dispatcher may feed.
- `h_dispatch(flock, args, self_pane_id)` — The deterministic feeder: standby queue + a tick that fills idle lanes.
- `h_clear_box(flock, args, self_pane_id)` — Destroy an unsubmitted draft.
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
- `h_report_run(flock, args, self_pane_id)`
- `h_report_list(flock, args, self_pane_id)`
- `h_owner_conversation(flock, args, self_pane_id)`
- `h_owner_reply(flock, args, self_pane_id)`
- `h_owner_ingest(flock, args, self_pane_id)`
- `h_owner_outbox(flock, args, self_pane_id)`
- `h_owner_delivered(flock, args, self_pane_id)`
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
- const `ID_PARAM` = `{"type": "string", "description": "Dunk id, e.g. 'd3' (see list_dunks)."}`
- const `TARGET_PARAM` = `{"type": "string", "description": TARGET_DOC}`
- const `TEXT_PARAM` = `{"type": "string", "description": TEXT_DOC}`
- const `INTERVAL_PARAM` = `{ "type": "number", "minimum": 0.01, "description": "Minutes between sends, greater than z`
- const `NAME_PARAM` = `{"type": "string", "description": "Optional human label for the dunk."}`
- const `ONLY_WHEN_PARAM` = `{ "type": "string", "enum": ["idle", "any"], "description": ( "'idle' holds each send unti`
- const `SKIP_IF_UNCONSUMED_PARAM` = `{ "type": "boolean", "description": ( "Backpressure: skip a scheduled send (no-op, send_co`
- const `HOLD_WHILE_TYPING_PARAM` = `{ "type": "boolean", "description": ( "Never type over a human: before each send the daemo`
- const `MAILBOX_PARAM` = `{ "type": "string", "description": ("Whose inbox: an agent name, a pane id, a unique label`
- const `MESSAGE_ID_PARAM_INBOX` = `{"type": "string", "description": "Inbox message id, e.g. 'i42', as returned by inbox_send`
- const `MESSAGE_IDS_PARAM` = `{"type": "string", "description": "One inbox message id, or several comma separated."}`
- const `MESSAGE_ID_PARAM` = `{"type": "string", "description": "Message id, e.g. 'm3' (see list_messages)."}`
- const `LABEL_PARAM` = `{"type": "string", "description": "Name to give it (how you will target it later)."}`
- const `CWD_PARAM` = `{"type": "string", "description": "Working directory for the new shell (default: the backe`
- const `RUN_COMMAND_PARAM` = `{ "type": "string", "description": ("Optional command line to type + Enter into the new pa`
- const `FOCUS_PARAM` = `{"type": "boolean", "description": "Switch the human's view to it (default false: create i`
- const `WORKSPACE_PARAM` = `{ "type": "string", "description": ("Workspace id ('w9'), label, unique label substring, o`
- const `TAB_PARAM` = `{ "type": "string", "description": "Tab id ('w9:t2'), label, unique label substring, or 's`
- const `KIND_PARAM` = `{"type": "string", "enum": ["workspace", "tab", "pane"], "description": "What `target` nam`
- const `KIND_TARGET_PARAM` = `{ "type": "string", "description": ("Id, label, unique label substring or 'self'. For pane`
- const `MAX_SENDS_PARAM` = `{ "type": "integer", "minimum": 0, "description": ( "Remove the dunk automatically after t`
- const `COMMANDS` = `[ Command( "owner_push", "Deliver an OWNER message to an agent with no inbox row and no ty`
- const `COMMANDS_BY_NAME` = `{command.name: command for command in COMMANDS}`
- `mcp_commands()`
- const `MCP_GROUPS` = `( ("inbox", "Agent-to-agent mailbox. action=send (preferred over typing), count, " "list, `
- class **GroupedTool** — One domain tool: an `action` enum plus the union of its commands' params.
  - `__init__(name, description, actions)`
  - `resolve(action)`
  - `input_schema()`
- `grouped_mcp_tools()`
- `mcp_tool_map()` — OLD flat command -> {tool, action, mcp}.
- const `MCP_TOOL_MAP` = `mcp_tool_map()`
- const `SUPERUSER_ONLY` = `{ "add_dunk", "update_dunk", "remove_dunk", "start_dunk", "stop_dunk", "toggle_dunk", "sto`
- `dispatch(flock, name, args, self_pane_id)` — Run a named command.

## dunk_callers.py (247 lines)

> Who is asking? A process identity for task leases, independent of herdr.  The pane id (HERDR_PANE_ID / TMUX_PANE) is the registry key. The liveness check

- const `AGENT_NAMES` = `frozenset({ "pi", "claude", "codex", "gemini", "cursor", "devin", "agy", "cline", "omp", "`
- const `MAX_DEPTH` = `12`
- const `DEAD_STATES` = `frozenset({"Z", "X", "x"})`
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

- const `DEFAULT_TTL_S` = `float(os.environ.get("DUNKINGSHEEP_CLAUDE_STATUS_TTL_S", "20"))`
- class **ClaudeStatus**
  - `__init__(ttl, binary, timeout)`
  - `agents(fresh)` — The live session list, cached for `ttl`; None when unavailable.
  - `session_for_pane(pane, fresh)` — The one session whose cwd is the pane's cwd, or None.
  - `status_for_pane(pane, fresh)`

## dunk_client.py (635 lines)

> Client for the Dunking Sheep daemon socket, plus daemon auto-start.      client = FlockClient.connect_or_start()

- const `IDEMPOTENT_READS` = `frozenset({ "status", "poll", "health", "help", "doctor", "whoami", "roster", "message_sta`
- const `HERE` = `os.path.dirname(os.path.abspath(__file__))`
- const `CLI_PATH` = `os.path.join(HERE, "dunkingsheep")`
- const `START_TIMEOUT_S` = `8.0`
- const `PROBE_RUNNING` = `"running"`
- const `PROBE_BUSY` = `"busy"`
- const `PROBE_ABSENT` = `"absent"`
- const `ABSENT_ERRNOS` = `{errno.ENOENT, errno.ECONNREFUSED, errno.ENOTSOCK, errno.EADDRNOTAVAIL}`
- const `BUSY_RETRY_DELAYS` = `(0.5, 2.0, 5.0)`
- const `SYSTEMD_UNIT` = `"dunkingsheep.service"`
- const `UNIT_PATH` = `os.path.expanduser("~/.config/systemd/user/dunkingsheep.service")`
- `_systemd_installed()` — True when the systemd user unit is installed.
- `_systemctl(*args)` — Run systemctl --user, supplying XDG_RUNTIME_DIR when the caller's shell does not have one (agents often do not).
- class **DaemonUnavailable** — The daemon is not running and could not be started.
- class **DaemonBusy** — The daemon is UP but did not answer in time.
- `_current_pane_id()` — The herdr pane this process is actually in, per herdr 0.9.5.
- `default_timeout(environ)` — The client's patience, from the environment or 45 s (t693).
- const `DEFAULT_TIMEOUT` = `default_timeout()`
- const `READ_RETRIES` = `1`
- class **IdentityRefused** — The caller's identity could not be established without guessing.
- `controlling_tty(pid, max_depth)` — The /dev/pts/N of the nearest process in this chain that has one.
- `pane_for_tty(tty, runner)` — (pane_id, reason): which tmux pane owns this terminal, or why we cannot say.
- `exported_pane_id(environ)` — The pane this process is actually in, per the terminal that owns it.
- `resolve_identity(explicit, environ, tty_lookup, detect, allow_unresolved)` — (pane_id, note) for this caller, refusing only a LYING environment.
- `_terminal_exists(lookup)` — True when the lookup's reason says a TERMINAL was found but did not resolve.
- `identity_refusal_for(cmd, self_pane_id, has_token, note)` — Why a pane-less caller may not run `cmd`, or None when it may (t602).
- class **FlockClient**
  - `__init__(socket_path, self_pane_id, timeout)`
  - `service_token()` — A service token for a pane-less process (web desk, timers), from DUNKINGSHEEP_SERVICE_TOKEN or the 0600 file DUNKINGSHEEP_SERVICE_TOKEN_FILE.
  - `call(cmd, **args)` — Send one command; return its result or raise DunkError / OSError.
  - `_call_once(request)`
  - `__getattr__(name)`
  - `probe()` — Classify the daemon WITHOUT side effects: running / busy / absent.
  - `is_running()`
  - `connect_or_start(cls, socket_path, self_pane_id, start, timeout)` — Return a client whose daemon is reachable, starting one if needed.
  - `start_daemon()` — Start the daemon: hand off to systemd when it supervises, else spawn a detached `dunkingsheep serve` that outlives this process.

## dunk_context.py (712 lines)

> The declarative context store: .dunkingsheep/ per project, one global.  Files are the source of truth (one Markdown entry with YAML-ish front matter).

- const `CLASSES` = `("ground-truth", "fact", "provisional", "history")`
- const `AUTHORITIES` = `("owner", "measured", "agent")`
- const `TYPES` = `("principle", "decision", "reference", "lesson", "measurement", "incident", "rule", "wiki"`
- const `OWNER_ONLY_TYPES` = `("rule", "principle")`
- const `LEDGER_PATH` = `Path(os.environ.get( "DUNKINGSHEEP_DIR", str(Path.home() / ".config" / "dunkingsheep"))) /`
- const `SCOPES` = `("global", "project")`
- const `DEFAULT_CAP` = `8192`
- const `PROVISIONAL_DAYS` = `7`
- const `GLOBAL_DIR` = `Path(os.environ.get( "DUNKINGSHEEP_DIR", str(Path.home() / ".config" / "dunkingsheep"))) /`
- class **ContextError** — A named refusal for the context store.
- `_sha(text)`
- class **Ledger** — What the daemon wrote: the table the digest trusts (FLAT_FLEET.md 8).
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
- `digest_both(project_path, which, max_bytes, subject, record)` — The session digest.
- `_stale_mark(entry)` — ' (STALE ...)' when an entry is past its recheck_after; ground truths too.
- `report(project_path, which)` — stats/check/index over every selected store, always labelled.
- class **Store**
  - `__init__(path, global_store, ledger)`
  - `_record(path, slug, meta, by)`
  - `verified(entry, ledger)` — Does the file on disk match what the daemon wrote?
  - `adopt(ledger, by)` — Bootstrap: record every live entry as-is.
  - `ensure(meta_lines)`
  - `exists()` — A store exists when it has a root with any class directory or README.
  - `entries()`
  - `find(subject_or_slug)`
  - `is_retired(entry)`
  - `live_entries()` — Current heads only; entries()/find() retain the historical audit trail.
  - `resolve(subject_or_slug)` — Resolve a subject or historical slug to its non-retired chain head.
  - `add(body, cls, subject, type_, authority, source, stated, recheck_after, supersedes, slug, by)`
  - `supersede(slug, new_body, by, **meta)`
  - `retire(slug, reason, by)`
  - `check()`
  - `index_path()`
  - const `INDEX_COLUMNS` = `("slug", "subject", "body", "class", "scope")`
  - const `_FTS5_HEADER` = `re.compile( r'^\s*CREATE\s+VIRTUAL\s+TABLE\s+["\[]?entries["\]]?\s+USING\s+fts5\s*\(', re.`
  - const `_PROBE` = `"SELECT slug FROM entries WHERE entries MATCH ? ORDER BY rank LIMIT 1"`
  - const `_PROBE_TERM` = `"probe"`
  - `index_compatible(path)` — True only when the cached index is an FTS5 table that can answer the search query this code runs.
  - `index()`
  - `search(query, cls, scope, limit)`
  - `access_path()`
  - `log_access(slug, by, kind)` — kind='read' when someone opened an article, 'injected' when it was delivered.
  - `reads()`
  - `stats()`
  - `entry_stats()` — Per-article access counts: what was delivered, what was opened, when.
  - `digest(max_bytes, subject)`

## dunk_core.py (9424 lines)

> Dunking Sheep core: the flock of dunkers and their timers.  This module is the single mechanism behind every control surface. The TUI's

- const `VERSION` = `"2.71.4"`
- `code_identity(path)` — Hash the flat application module tree, not just dunk_core.py.
- const `CODE_IDENTITY` = `code_identity()`
- `code_is_stale(identity, current)` — Compare the import-time application tree with the current disk tree.
- `code_status()` — One disk read per status request, including names and read failures.
- const `TASK_LEADER_ENV` = `"DUNKINGSHEEP_LEADER_PANE"`
- const `CONFIG_DIR` = `os.environ.get("DUNKINGSHEEP_DIR") or os.path.expanduser( "~/.config/dunkingsheep" )`
- const `STATE_FILE` = `os.path.join(CONFIG_DIR, "dunks.json")`
- const `LAUNCHES_FILE` = `os.path.join(CONFIG_DIR, "launches.json")`
- const `BUSY_STATUSES` = `("working", "blocked")`
- const `_PANE_ID_RE` = `re.compile(r"^(?:[A-Za-z0-9_.-]+:p\d+|%\d+)$")`
- const `_HERDR_ID_RE` = `re.compile(r"^w\d+:p[A-Za-z0-9_.-]+$")`
- const `PANE_ID_RE` = `_PANE_ID_RE`
- const `_TERM_ID_RE` = `re.compile(r"^term_[0-9A-Za-z]+$")`
- const `_EMBEDDED_PANE_RE` = `re.compile(r"\(\s*([A-Za-z0-9_.-]+:p\d+|%\d+)\s*\)\s*$")`
- const `IDLE_POLL_S` = `5`
- const `MIN_IDLE_POLL_S` = `0.5`
- `idle_poll_wait(stop_event, idle_poll_s)` — One idle-poll wait, floored (t212).
- const `RESTORE_GRACE_S` = `5`
- const `DEFAULT_TEXT` = `"continue"`
- const `DEFAULT_INTERVAL_MINUTES` = `10.0`
- const `ONLY_WHEN_CHOICES` = `(None, "idle")`
- const `TASK_REAP_S` = `float(os.environ.get("DUNKINGSHEEP_REAP_S", "15"))`
- const `TURN_TTL_S` = `float(os.environ.get("DUNKINGSHEEP_TURN_TTL_S", "1800"))`
- const `TURN_END_DISPATCH` = `os.environ.get( "DUNKINGSHEEP_TURN_END_DISPATCH", "1").strip().lower() not in ( "0", "fals`
- const `TURN_END_WAIT_S` = `float(os.environ.get("DUNKINGSHEEP_TURN_END_WAIT_S", "4"))`
- const `MAILBOX_CACHE_S` = `float(os.environ.get("DUNKINGSHEEP_MAILBOX_CACHE_S", "30"))`
- const `VIEW_CACHE_S` = `float(os.environ.get("DUNKINGSHEEP_VIEW_CACHE_S", "5"))`
- const `TASK_PUSH_S` = `float(os.environ.get("DUNKINGSHEEP_PUSH_S", "10"))`
- const `CONSUMPTION_READ_LINES` = `40`
- const `CONSUMPTION_PREFIX_CHARS` = `80`
- const `INPUT_BOX_SIGILS` = `("\u276f", "\u203a", ">")`
- const `MAX_UNKNOWN_SKIPS` = `3`
- const `COLLAPSED_INPUT_MARKERS` = `( "ctrl+o to expand", "paste again to expand", "pasted text", "pasted content", "[image", `
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
- const `MODAL_MARKERS` = `( "esc to cancel", "esc to close", "esc to exit", "enter to confirm", "press enter to cont`
- const `NOTICE_PREFIX` = `"[inbox] "`
- const `DRAFT_CONFIRM_S` = `0.5`
- const `MODAL_WAIT_S` = `30.0`
- const `ENTER_KEYS` = `("Enter", "enter", "Return", "return", "KP_Enter")`
- const `SUBMIT_ATTEMPTS` = `len(SUBMIT_GAPS)`
- const `DEAD_SESSION_MARKERS` = `( "server connection could not be restored", "automatic reconnect could not restore this s`
- const `HARNESS_QUEUES_WHILE_BUSY` = `{"claude": True, "pi": True, "codex": True}`
- const `DELIVERY_TAIL_LINES` = `12`
- const `STATUS_TAIL_LINES` = `3`
- const `WORKING_MARKERS` = `("esc to interrupt", "esc to cancel", "ctrl+c to interrupt")`
- const `PAUSED_GOAL_MARKERS` = `("goal paused", "paused (/goal resume)", "/goal resume")`
- const `CONTEXT_FULL_MARKERS` = `("/clear to save", "context is full", "context limit reached")`
- const `PLACEHOLDER_PATTERNS` = `( re.compile(r"new task\?\s*/clear to save [\d.,]+[km]? tokens?", re.I), re.compile(r"^\s*`
- const `CLAUDE_BUSY_TTL_S` = `float(os.environ.get("DUNKINGSHEEP_CLAUDE_BUSY_TTL_S", "600"))`
- const `TYPING_LOCK_TIMEOUT_S` = `float(os.environ.get("DUNKINGSHEEP_TYPING_LOCK_TIMEOUT_S", "10"))`
- const `TYPING_LOCK_LOUD_S` = `float(os.environ.get("DUNKINGSHEEP_TYPING_LOCK_LOUD_S", "0.25"))`
- const `BOX_HOLD` = `"the box would not clear (text left untouched)"`
- const `SUBMIT_CONFIRM_S` = `2.0`
- const `SUBMIT_POLL_S` = `(0.1, 0.2, 0.4, 0.8, 1.0)`
- const `SUBMIT_VERIFY_GAPS` = `(0.4, 1.0)`
- const `TEMPLATE_RE` = `re.compile(r"\{(id|name|count|target|interval|time|date)\}")`
- const `ANSI_RE` = `re.compile(r"\x1b\[[0-9;?]*[ -/]*[@-~]")`
- const `DECORATION_RE` = `re.compile(r"[─-▟⠀-⣿\s]+")`
- class **_NoticeFolded** — Internal: a notice was merged into one already waiting.
- const `SYSTEM_SENDERS` = `("watchdog",)`
- class **DunkError** — A caller mistake (bad id, bad interval, ambiguous target).
- class **RetryableDunkError** — Safe to show, and the caller may retry the request (busy store, not refusal).
- class **AmbiguousTarget** — A target matched more than one pane.
- `_note_text(note)` — The evidence text of one task note, whichever key it carries (t704).
- `_as_dunk_error(method)` — An unknown message id is a caller mistake like any other, so it should come back as a plain message rather than as an internal error.
- `_truthy(value, default)`
- `format_interval(minutes)` — Human form of an interval: '10', '1.5', or '30s' for under a minute.
- `parse_interval(value)` — Accept minutes as a number or a string like '60', '1.5h', '90s', '2m'.
- `expand_template(text, dunker)` — Expand the known `{placeholders}` in `text`; leave other braces alone.
- class **Dunker** — One dunk.
  - const `FIELDS` = `( "id", "name", "target_pane_id", "target_agent", "target_tab", "target_workspace", "targe`
  - `__init__(id, **kwargs)`
  - `target_label()`
  - `set_target(pane)` — Point at a terminal pane dict (as returned by the backend's list_panes_grouped).
  - `to_dict()`
  - `from_dict(cls, data)`
- class **Message** — One direct message waiting in (or delivered from) a pane's outbox.
  - `__init__(id, pane_id, target_label, text, source, deliver)`
  - `_queued_message()`
  - const `OUTCOMES` = `("ok", "stored", "queued", "unconfirmed", "refused")`
  - `outcome()`
  - `to_dict()`
- class **Flock** — Owns every dunker and its timer thread.
  - `__init__(herdr, state_file, launches_file, persist, restore, inbox, metrics, tasks, superusers, memory, stuck_config, roles)`
  - `_stuck_config_source(explicit)` — Where the runtime stuck-recovery config comes from, in one documented order.
  - `_restore()`
  - `_save()`
  - `_load_launches()`
  - `_restore_outbox(rows)` — Rebuild held messages after a restart and log what came back.
  - `_save_launches()`
  - `_record_launch(pane_id, **fields)`
  - `_snapshot_launches(panes)` — Keep launches.json current from herdr's agent_session metadata.
  - `_find(dunk_id)`
  - `get(dunk_id)`
  - `list()`
  - `status(check_herdr)`
  - `poll_status()` — The cheap half of status: counts only, no global inbox/task scans.
  - `summary()`
  - `registry_pane_gap()` — Registered lanes that the terminal backend does not list (t714).
  - `list_panes(self_pane_id)`
  - const `IDLE_CLAIM_S` = `dunk_activity.IDLE_CLAIM_S`
  - `lane_status(pane)`
  - `lane_activity(pane)` — What the lane is DOING, from the best evidence this backend has.
  - `_quarantine_shared_mailboxes()` — Move mail that an older resolver filed under an agent *type* out of reach.
  - `pm_digest(project, payload, to, window, self_pane_id)` — Publish ONE digest per project under a stable id.
  - `resolve_mailbox(spec, self_pane_id, strict)` — Turn a target into (mailbox key, label, pane id).
  - `_is_agent_type(spec)` — True if `spec` names a kind of agent (claude, codex) rather than one of them, so it must never become a mailbox.
  - `_pane_refusal(pane_id, spec)` — The send-time refusal for a pane id the backend does not list.
  - `_registered_pane(pane_id)` — (registered, process_alive, pid) from the role registry.
  - `_mailbox_for_pane(pane)`
  - `view(self_pane_id, fresh)` — Cheap view for the poll-shaped commands: status + dunks + inbox count.
  - `_invalidate_view()`
  - `_own_mailbox(self_pane_id)` — The caller's mailbox key, briefly cached.
  - `lane_registry_path()`
  - `_lane_registry(strict)`
  - const `_PLAIN_LANE_NAME` = `re.compile(r"^[A-Za-z0-9][A-Za-z0-9._:-]*$")`
  - const `_NOT_A_NAME_MARKERS` = `("[inbox]", "read it with", "|", "\u2502", "\u203a")`
  - `_plain_lane_name(cls, name)` — True when this string may become a lane's identity BY INFERENCE.
  - `_usable_declared_name(cls, name)` — True when a DECLARED row holds something a lane could be called.
  - `_row_identity(value)` — (mailbox, declared) for a registry row, however it is spelled.
  - const `DRAIN_HOOKS` = `{"pi": "owner_push"}`
  - const `DRAIN_FRESH_S` = `300`
  - `delivery_capability(target, self_pane_id)` — How the desk may deliver to this target, and what it may show.
  - `_desk_send_rights(pane_id)` — Does the desk credential exist, and does it cover THIS pane?
  - `delivery_target(target, self_pane_id)` — Resolve a DIRECT-DELIVERY target (owner push, keystrokes) to one pane.
  - `lanes()` — The lane registry as rows: pane id, mailbox, declared?, alive?
  - `lane_set(pane_id, mailbox, self_pane_id)` — Declare who a pane IS, after the fact.
  - `_remember_lane(pane_id, mailbox, declared)` — Record pane id -> mailbox, so a backend flip cannot orphan a lane's mail.
  - `_write_lane_registry(data)` — Atomic replace of the lane registry.
  - `_title_pane(pane_id, mailbox)` — Best effort: put the lane name in the pane title too.
  - `_lane_pane(mailbox)` — (live pane, recorded mailbox) for a mailbox in the lane registry, else (None, None).
  - `_registered_lane(name)` — (found, alive, dead_since) for a mailbox name in the lane registry.
  - `_mailbox_exists(name)` — True when the inbox already holds mail addressed to this name.
  - `_mailbox_or_self(spec, self_pane_id, strict, own_only)`
  - `inbox_send(to, body, subject, attach, notify, self_pane_id, system_sender)` — Put a message in another agent's inbox.
  - `inbox_archive(mailbox, before, states, self_pane_id)` — Bulk-retire a mailbox: mark messages read and archived, never delete.
  - `_notice_for(mailbox, unread, sender, subject)`
  - `_refresh_queued_notice(pane_id, text)` — If an inbox notice for this pane is still waiting, update it to the new unread count and return it.
  - `inbox_count(mailbox, self_pane_id)`
  - `inbox_list(mailbox, unread_only, limit, self_pane_id)`
  - const `OWNER_MAILBOX` = `"owner"`
  - `_owner_role(self_pane_id)` — Only the desk and the owner's own transport read or answer the owner.
  - `owner_conversation(limit, before, self_pane_id)` — The owner mailbox as one conversation: questions and their answers.
  - `owner_ingest(body, transport, external_id, subject, attachments, self_pane_id)` — Record one owner question that arrived on `transport` (t594).
  - `owner_outbox(transport, limit, self_pane_id)` — Replies waiting for `transport` to deliver them (t594).
  - `owner_delivered(message_id, transport, external_id, self_pane_id)` — Record a transport's delivery receipt for one owner row (t594).
  - `owner_reply(message_id, body, self_pane_id)` — Answer one owner question: one reply row, linked, on the same transport.
  - `inbox_read(mailbox, limit, peek, self_pane_id)` — Read a batch WITHOUT consuming it, build the whole result, then commit.
  - `inbox_recover(mailbox, self_pane_id)` — Replay the latest delivered batch for display after a lost response.
  - `inbox_status(message_id)`
  - `inbox_ack(message_ids, mailbox, self_pane_id)` — Ack only messages in the caller's own mailbox.
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
  - `task_read(task_id, self_pane_id)` — One task, with the last few note reasons attached (t605).
  - `task_update(task_id, title, body, status, priority, parent_id, assignees, depends_on, block_reason, multi_assignee, self_pane_id, updated_by)`
  - `task_assign(task_id, assignees, mode, self_pane_id)`
  - `task_complete(task_id, by, self_pane_id, verdict)`
  - `_require_completion_binding(self_pane_id)` — Refuse a pane claim the kernel evidence does not support (t423 P1).
  - `_authorize_task_complete(task, self_pane_id, operator)` — Refuse foreign completion BEFORE any mutation, event or handoff (t423).
  - `task_block(task_id, reason, self_pane_id)`
  - `task_unblock(task_id, self_pane_id)`
  - `task_cancel(task_id, reason, self_pane_id)`
  - `task_delete(task_id, self_pane_id)`
  - `task_note(task_id, event, reason, self_pane_id)` — Append an explanatory event (e.g.
  - `require_turn_caller(pane_id, self_pane_id)` — Authenticate socket turn events; request pane labels alone prove nothing.
  - `turn_start(pane_id, ttl_s, source, self_pane_id, handoff_id)` — Declare a busy turn for `pane_id` (pi/claude hook or extension).
  - `turn_end(pane_id, reason, self_pane_id, turn_id, authenticated)`
  - `_dispatch_on_turn_end(pane_id)`
  - `_dispatch_claimed_turn_end(pane_id)` — Prompt a lane that ended its turn still holding work.
  - `turn_active(pane_id)` — True only for an unexpired declared turn.
  - `turn_expire(now)` — Release expired turns: record the reason, reconcile the caller pid.
  - `set_caller(caller)`
  - `current_caller()`
  - `clear_caller()`
  - `set_socket_peer(ok)` — Record whether this request arrived over the socket with verifiable peer credentials (t423).
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
  - `memory_backfill(action, limit)` — Discover every known session source and enqueue it (throttled, resumable, pausable).
  - `memory_ingest(path, harness, project)`
  - `health(repair, deep)` — Deterministic self-check; see dunk_health.run_health.
  - `_start_task_workers()`
  - `_clear_historical_errors(panes)` — Clear a stale 'target pane not found' once the pane is back.
  - `_archive_retired_inbox(pane_list)` — Revalidate retirement and bindings at mutation; uncertain identity stays queued.
  - `_task_reaper_loop()`
  - `_task_push_loop()`
  - `_push_tasks()` — Feed idle followers one task each, using the same idle signal as `only_when="idle"` and the same guarded inbox nudge as everything else.
  - `_announce_task(pane_id, task, lease_seconds)`
  - `metrics_summary(window_s, since, until)` — A read-only snapshot of how delivery is going, for a status board.
  - const `LAYOUT_KINDS` = `("workspace", "tab", "pane")`
  - `list_workspaces()` — Workspaces with their tabs nested, sorted by number.
  - `_self_pane(self_pane_id)`
  - `_pick(items, spec, kind, id_key, label_key)` — Resolve `spec` against a list: exact id, exact label (case- insensitive), then unique substring of the label.
  - `resolve_workspace(spec, self_pane_id)` — Workspace dict from an id ('w9'), a label, a unique label substring or 'self' (the caller's own workspace).
  - `resolve_tab(spec, self_pane_id)` — Tab dict from an id ('w9:t2'), a label, a unique label substring or 'self' (the caller's own tab).
  - `_pane_info(pane_id)`
  - `_role_pane_info(pane_id)` — Pane record for role binding, cached 2 s: one tmux listing per request under load is what made every read cost 3-9 s (t535).
  - `caller_role(self_pane_id)` — The caller's role for this request: token first, then the registry.
  - `set_service_token(token)`
  - `is_superuser(self_pane_id)`
  - `require_superuser(self_pane_id, action)` — Fail closed: only the owner role may do destructive things.
  - `whoami(self_pane_id)`
  - `lane_register(pane, role, provider, model, label, self_pane_id)` — Bind a role to a pane's root process.
  - `lane_unregister(pane, self_pane_id)`
  - `token_issue(role, label, self_pane_id)` — Mint a service token for a pane-less process (web desk, timers).
  - `token_revoke(label, self_pane_id)`
  - `_github_tokens()`
  - `gh_token(kind, self_pane_id)` — A short-lived installation token for the App this caller's role implies.
  - `_post_report(report, text)` — Put the report in the owner mailbox on the `report` transport.
  - `report_run(post, prose, out_dir, self_pane_id)`
  - `report_list(out_dir, self_pane_id)`
  - `review_sync(dry_run, self_pane_id)`
  - `poolrad_sync(limit, dry_run, self_pane_id)`
  - `poolrad_progress(self_pane_id)`
  - `review_list(self_pane_id)`
  - `_require_review_evidence(task, self_pane_id)` — Refuse a verdict unless this pane noted its evidence after it claimed.
  - `_review_evidence(task)` — The reviewer's own note text, verbatim, for the follow-up and GitHub.
  - `_after_review_verdict(task, verdict, by)`
  - `_context_store(global_store, project_path)`
  - `context_add(body, cls, subject, type_, authority, source, stated, recheck_after, supersedes, global_store, project_path, self_pane_id)` — Write an entry through the daemon.
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
  - `create_workspace(label, cwd, command, focus, mailbox, role, provider, model, self_pane_id)` — New workspace (with its first tab and pane).
  - `create_tab(workspace, label, cwd, command, focus, self_pane_id)` — New tab.
  - `split_pane(target, direction, cwd, command, focus, self_pane_id)` — Split an existing pane (any pane target) right or down.
  - `_sandbox_workspace_id()` — The dedicated workspace where spawned agents go by default.
  - `start_agent(name, command, workspace, tab, split, cwd, focus, self_pane_id, role, provider, model)` — Launch `command` in a new pane registered by name as agent `name` (on the herdr rollback: `herdr agent start`).
  - `respawn_pane(pane_id, argv, rollback_argv, expected_pid, expected_start, confirm_idle, reason, self_pane_id)` — Replace a managed pane's process in place, session-preserving.
  - const `AGENT_VERIFY_S` = `float(os.environ.get("DUNKINGSHEEP_AGENT_VERIFY_S", "2.0"))`
  - const `AGENT_FAILURE_MARKERS` = `( "command not found", # bash/zsh: "foo: command not found" ": not found", # dash/sh: "/bi`
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
  - const `HERDR_BLOCKED` = `( (), ("server", "stop"), ("update",), ("channel", "set"), ("session",), ("completion",), `
  - const `HERDR_TYPING` = `{ ("pane", "send-text"): "send_text(target=..., text=...)", ("pane", "send-keys"): "send_t`
  - `herdr_command(command, self_pane_id)` — Run any `herdr <subcommand ...>` and return its parsed result.
  - `herdr_help(topic)` — herdr's own usage text: the command groups (`topic` empty) or one group's subcommands (`topic` = 'pane', 'workspace', 'tab', 'agent', 'wait', 'notification', ...).
  - `resolve_target(spec, self_pane_id)` — Turn a target spec into a pane dict.
  - `add(target, text, interval_minutes, name, start, only_when, max_sends, skip_if_unconsumed, hold_while_typing, self_pane_id)`
  - `update(dunk_id, target, text, interval_minutes, name, only_when, max_sends, skip_if_unconsumed, hold_while_typing, self_pane_id)` — Change fields on a dunk.
  - `remove(dunk_id)`
  - `start(dunk_id)`
  - `stop(dunk_id)`
  - `toggle(dunk_id)`
  - `stop_all()`
  - `_start(dunker)`
  - `_spawn_timer(dunker)`
  - `_halt(dunker)` — Stop the timer thread without touching the persisted running flag.
  - `fire(dunk_id, countdown_s, wait)` — Send a dunk's text right now (the classic 't' test send).
  - `_fire_worker(dunker, countdown_s)`
  - `send_now(target, text, self_pane_id, hold_while_typing, wait_s, press_enter, is_notice, notify_for, source)` — One-off send to any pane, no dunk involved.
  - `send_keys_now(target, keys, self_pane_id, hold_while_typing, wait_s)` — Press keys in a pane, queued behind a human who is typing exactly like a message.
  - `_new_message(pane_id, label, text, source, deliver)`
  - `_deliver(message, now)` — Send one message, re-checking the box under the send lock.
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
  - `_stuck_status(pane_id)` — The delivery state for the recovery decision: deliverable is not enough, a busy/unknown pane must RESET the clock rather than be acted on.
  - `_pane_generation(pane_id)` — Pane identity that changes when a pane is replaced: pid AND process start.
  - const `STUCK_OBSERVE_TICK_S` = `5.0`
  - `_federated_composer(screen)` — Authorized live input under the explicit all-input-via-DS policy.
  - `_federated_recovery_step(now)` — Deterministic preserve-submit, using the existing durable attempt ledger.
  - `_dialog_observer_step(now)` — Scan live agent dialogs every 30s even without queued mail.
  - `_stuck_observer_step(now)` — ONE periodic scan for a stalled delivery nobody else is watching (t377).
  - `_stuck_observer_loop()`
  - `_outbox_worker(pane_id)` — Deliver a pane's queued messages in order, one deterministic step at a time.
  - `_outbox_step(pane_id, now)` — ONE iteration of the outbox worker.
  - `list_messages(why_blocked)` — Queued messages (oldest first) and the recent delivered/failed ones.
  - `get_message(message_id)`
  - `cancel_message(message_id)` — Drop a queued message before it is delivered.
  - `read_pane(target, lines, self_pane_id)`
  - `_do_send(dunker)`
  - `_previous_send_consumed(dunker)` — Decide, at send time, whether the previous send has left the target's input box.
  - `read_screen(pane_id, lines, ansi)` — The one way a component asks the pane what it looks like.
  - `read_box(pane_id)` — (screen, text, detail) for a pane's input box, from one read.
  - `_typing_in(pane_id, last_sent_text, awaiting_consumption)` — True if someone is composing text in `pane_id`'s input box.
  - `send_blocker(pane_id, last_sent_text, awaiting_consumption)` — Why a send cannot go now, as (state, reason), or None if it can.
  - `_pane_harness(pane_id)`
  - `_clear_to_send(pane_id, last_sent_text, awaiting_consumption, stop_event)` — True once the box has read clear on two checks TYPING_SETTLE_S apart, so a pause between thoughts is not mistaken for being done.
  - `_box_holds(pane_id, text)` — True if `text` still sits in the pane's input box, False if nothing of ours is waiting there, None if the pane could not be read.
  - `_session_process_gone(pane_id)` — True when the lane's own recorded process is no longer running.
  - `_dead_session(pane_id)` — Return the dead-session marker showing at the BOTTOM of the pane, or None.
  - `_send_submit_key(pane_id, fallback, busy_hint)` — Submit using the harness's current queue action, under the caller's lock.
  - `_flush_once(pane_id, text)` — One Enter, one short check.
  - `_press_until_submitted(pane_id, text, first_pressed, busy_hint, baseline)` — Press Enter (each press under the typing mutex) until `text` leaves the box.
  - `_read_status(pane_id)` — Quiet status read: None on any probe failure (never fails a send).
  - `_turn_started(pane_id, pre_status)` — True when a turn STARTED during this send: working now, not working before.
  - `_remember_typed(pane_id, text)`
  - `_looks_like_ours(pane_id, shown)` — True if what is in the box is text we typed.
  - `_region_is_ours(pane_id, region)` — t130: STRICT ownership for the ambiguous-frame flush.
  - `_composer_ownership(pane_id, screen)` — (owned, region) for an ambiguous frame - THE shared decision path.
  - `composer_region_is_owned(pane_id, screen)` — PUBLIC (t377 seam, Owner2282): the exact-owned full-composer check.
  - `_dim_bodies_accounted(pane_id, screen)` — Account the COMPLETE excluded span, never only a sigil's first row.
  - `_box_text(pane_id)` — Whatever is sitting in the pane's input box: '' when empty, None when there is no box to read.
  - `_recovery_box_text(pane_id, screen)` — Unknown geometry is never evidence that the prompt was consumed.
  - `press_keys(pane_id, keys)` — Press a sequence, under the global typing mutex (one burst).
  - `_press_keys_unlocked(pane_id, keys)` — Press keys in a pane.
  - `submit_text(pane_id, text, press_enter, check_box)` — Type `text` into a pane, press Enter, and confirm it left the box.
  - `flush_box(target, self_pane_id)` — Submit whatever is sitting unsent in a pane's input box.
  - `_box_signature(pane_id)` — What the pane is showing, as ("modal"|"box", text).
  - `_note_stale_panel(pane_id, signature, since)` — Say once that a dialog is holding up this pane.
  - `_draft_persists(pane_id)` — Read the box again after a moment and return what is still there.
  - `resolve_lanes()` — Every pane that has an agent in it: the set the dispatcher may feed.
  - `delivery_state(pane_id, status, pane, screen)` — Classify whether this pane can receive a message, and why not.
  - `clear_box(target, self_pane_id, reason)` — CLEAR an unsubmitted draft (Ctrl-U, Escape).
  - `_ensure_box_clear(pane_id, allow_modal, preserve_draft)` — Make the input box ready to type into.
  - `_lock_for(pane_id)`
  - `send_guarded(pane_id, send, last_sent_text, awaiting_consumption, hold_while_typing, allow_modal, preserve_draft, permission)` — Run `send` (a callable that types into `pane_id`) with the send lock held, after one final look at the input box under that same lock.
  - `_box_check(enabled, allow_modal, already_checked)` — Tell the typing primitive how this send treats the box.
  - `_typing(pane_id, timeout_s)` — The global typing mutex, held for one keystroke burst.
  - `_claude_busy(pane_id, pane)` — True only when Claude's own session reports status=busy AND that report is fresh enough to believe.
  - `_pane_is_quietly_idle(pane_id)` — True only when the pane itself says nothing is happening.
  - `_claim_names_a_dead_session(session)` — A sentence naming why a busy claim is WRONG rather than stale, or None.
  - `_human_typing(dunker)` — True if someone is composing text in the target's input box right now, so a send would splice our text into the middle of theirs and Enter would submit the mangled result.
  - `_timer_loop(dunker, stop_event)`
  - `close()` — Halt every timer thread, keeping persisted running flags so a new Flock resumes them.
- `_intent_order(entry)` — Order two delivery intents: created_at, then the message id as a tie-break.
- `_hold_backoff_s(attempts)` — Exponential backoff for a refused delivery: base doubling, capped.
- `_looks_like_usage(text)` — herdr prints usage for a bare command group (exit non-zero); that is an answer, not an error.
- `_describe(panes)`
- `_numeric_id(dunk_id)`
- `_check_only_when(value)`
- `_collapse(text)` — Whitespace-insensitive form of text, so wrapped lines still match.
- `_is_border_line(line)`
- `_bordered_input_area(tail)` — The pi TUI has no prompt sigil: its input is the line(s) between the last two full-width box borders, with the cwd/model footer below.
- `_input_area(tail)` — Return the agent's input-box region: everything from the last prompt sigil line (Claude ❯, Codex ›) to the end of the visible screen.
- `_input_pending(tail, last_sent_text)` — True if the target's input box still holds unconsumed input, False if it is clear, None if we cannot tell (unreadable pane or no recognisable box).
- `_strip_ansi(text)` — Plain text of an ANSI-styled screen.
- `_sgr_dim(params, dim)` — Apply one SGR parameter list to the current dim state.
- `_bright_text(line)` — The characters of one ANSI-styled line that are not rendered dim.
- `_notice_text(mailbox, unread, sender, subject)` — The line typed into a pane to say mail has arrived.
- `_banner_marker(screen, markers)` — A marker that starts its own line is a banner; one mid-sentence is prose.
- `_is_placeholder(text)` — True when a line is the harness's empty-box hint rather than a state.
- `_status_marker(screen, markers, lines, placeholder_safe, dim_ok)` — A status decoration on the composer line: '...
- `delivery_rows(screen)` — Non-blank rows with colour codes removed.
- `delivery_tail(screen, lines)` — The last non-blank lines of a pane, lowercased: where state banners live.
- `classify_delivery(screen, status, typed, our_text, awaiting_consumption)` — Why a pane can or cannot receive a message, as (state, reason).
- `_modal_open(screen)` — True if the pane is showing a panel or dialog *right now*.
- const `COMPOSER_HINT_TAIL_ROWS` = `5`
- const `BOX_REGION_ROWS` = `14`
- const `AMBIGUOUS_COMPOSER` = `"ambiguous composer/transcript:"`
- const `MID` = `" \u00b7 "`
- const `BOX_LEADING` = `" \t\u2502\u2503\u2506\u250a\u2551|"`
- `_prompt_body(row)` — The text after the prompt sigil on one row, or None if the row has no sigil.
- const `NATIVE_BANNER_RE` = `re.compile(r"^>_\s+OpenAI Codex(?:\s+\(v[0-9][0-9.]*\))?$")`
- const `NATIVE_STATUS_RE` = `re.compile(r"^\S[\w .+/-]{0,40}\s\u00b7\s(?:~|/)\S*$")`
- const `NATIVE_HINTS_RE` = `re.compile(r"^(?:\? for shortcuts|\u2190 for agents)\b")`
- const `NATIVE_WARNING_RE` = `re.compile(r"^\u26a0\s+\d+\s+warnings?\s+\u00b7\s+f2 to view$")`
- `_native_banner(row)`
- `_native_status(row)`
- `_native_hints(row)`
- `_native_warning(row)`
- `_native_footer_start(raw, nonblank)` — The index of the native two-row footer's first row, or None.
- `_styled_sigil_state(row)` — Classify a prompt-sigil row by the SGR state IN EFFECT AT THE SIGIL POSITION (t130, %19 review i31909: deciding from the union of codes in the row misclassifies nested styles in both directions - a dim sigil with a bright body, and a live composer row carrying a single dim word).
- `_styled_composer_boundary(screen)` — ("delimited", region) or ("undelimitable", "") using CAPTURED STYLE STATE, for the multi-sigil frames the plain boundary fails closed on.
- `_trailing_prompt_region(screen)` — Complete trailing region for the native-bound recovery path only.
- `_native_prefix_accounted(screen, native_text)` — An earlier prompt or human continuation cannot disappear at extraction.
- `_composer_region_text(screen)` — The live composer's visible content, or "" when it cannot be delimited (FAIL CLOSED - CoS i31543/i31555: never submit on an undelimited frame).
- `box_text(screen)` — What is in the input box, and how we know, as (text, detail).
- `_typed_text(screen)` — What a human has typed into the target's input box, judged from an ANSI-styled screen: the non-dim text on the last prompt-sigil line, minus the sigil and box decoration.
- `_is_our_send(typed, last_sent_text, awaiting_consumption)` — True if the text in the box is our own previous send rather than a human's draft: the sent text (the visible line is a prefix of it, or starts with its prefix), or a collapsed-paste placeholder while our last send is still unaccounted for.
- `_check_bool(value, name)`
- `_check_max_sends(value)`

## dunk_desk.py (106 lines)

> A desk credential: exactly one permission, and it is not a superuser entry.  The web desk must be able to type into a lane the owner is looking at. Granting it

- const `CONFIG_DIR` = `Path(os.environ.get("DUNKINGSHEEP_DIR", str(Path.home() / ".config" / "dunkingsheep")))`
- const `PATH` = `CONFIG_DIR / "desk.json"`
- const `DESK_VERBS` = `frozenset({"send_text", "send_keys"})`
- class **DeskError** — A named refusal: the caller needs to know which rule stopped it.
- `_read(path)`
- class **DeskCredential** — Token -> allowed pane set.
  - `__init__(path)`
  - `issue(panes)` — Create (or rotate) the token for this pane set.
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

- const `CONFIG_DIR` = `os.environ.get("DUNKINGSHEEP_DIR", os.path.expanduser("~/.config/dunkingsheep"))`
- const `DB_PATH` = `os.path.join(CONFIG_DIR, "dispatch.db")`
- const `STATE_PATH` = `os.path.join(CONFIG_DIR, "dispatch-state.json")`
- const `SCHEMA` = `""" CREATE TABLE IF NOT EXISTS standby ( id INTEGER PRIMARY KEY AUTOINCREMENT, task_id TEX`
- `_now()`
- class **DispatchError** — A named refusal.
- class **Queue** — The standby list: ordered assignments any ready lane can take.
  - `__init__(path)`
  - `add(brief, task_id, lane)`
  - `get(item_id)`
  - `items(pending_only)`
  - `remove(item_id)`
  - `take(item_id, lane, sent_message, now)`
  - `next_for(lane)` — The first pending item assigned to this lane, else the first unassigned.
- `lane_plan(panes, delivery, unread, lanes)` — Classify every lane for one tick.
- const `PRIORITY_ORDER` = `{"urgent": 0, "high": 1, "medium": 2, "low": 3}`
- const `HELD_PREFIX` = `"HELD FOR OWNER DECISION"`
- `derive_board_items(tasks, dispatched_task_ids, now, coarse_chars)` — Standby items DERIVED from the task board.
- `dispatch_text(item)` — What the lane is sent.
- const `TURN_END_MIN_GAP_S` = `float(os.environ.get("DUNKINGSHEEP_TURN_END_MIN_GAP_S", "30"))`
- const `TURN_END_TASK_COOLDOWN_S` = `float( os.environ.get("DUNKINGSHEEP_TURN_END_TASK_COOLDOWN_S", "300"))`
- `claimed_tasks(tasks, pane_id)` — The tasks `pane_id` has CLAIMED (holds a live lease on), in the order to offer them.
- `claimed_work(tasks, pane_id)` — The first task the lane has claimed (see `claimed_tasks`), or None.
- `turn_end_plan(pane_id, tasks, unread, last_nudge_at, task_nudges, now, min_gap_s, task_cooldown_s)` — Decide whether a turn end earns a prompt.
- `turn_end_text(task)` — What a lane that ended its turn holding work is sent.
- `tick(queue, panes, delivery, unread, send, now, dry_run, lanes, state_path, one_per_lane, derived)` — Fill idle lanes from the standby queue.
- `_dispatched_in_window(state_path, now, window_s)` — How many sends this hour, from the history this module keeps.
- `_load_state(path)`
- `_write_state(state, path)` — Atomic, and it keeps a small history so 'dispatched this hour' is real.
- `summary(state)` — The three numbers the owner asked for, one line each.
- `next_job_plan(trigger, pane_id, tasks, unread, pending_delivery, last_nudge_at, task_nudges, now, min_gap_s, task_cooldown_s, dispatch_reason)` — Decide what a lane does the moment it goes free.
- `assigned_open_tasks(tasks, pane_id)` — Open tasks ASSIGNED to `pane_id`, in board order, excluding blocked/gated ones.
- `delivery_retry_plan(attempt, last_at, now, max_attempts, min_gap_s)` — One bounded retry policy, with the dispatch rate floor and submit ceiling.

## dunk_fallback.py (370 lines)

> t359: deterministic model-fallback planning.  The owner's spec (i2249/i2250) asks for a function that, given the SAME credit

- const `TIER_ORDER` = `("claude", "codex", "pi", "free")`
- const `REQUIRED_WINDOWS` = `{ "claude": ("five_hour", "weekly_all"), "codex": ("weekly",), "opencode-go": ("five_hour"`
- const `FREE_PROVIDER` = `"free"`
- const `TIER_PROVIDER` = `{ "claude": ("claude",), "codex": ("codex",), # The board's "pi" reading is a LOCAL SESSIO`
- const `HARNESS_WIRING` = `{ "codex": { "args": ["-s", "danger-full-access", "-a", "never"], # t310/t264 recorded tha`
- const `SPENT_PERCENT` = `0.1`
- const `RESERVE_PERCENT` = `5.0`
- class **FallbackError** — A caller error, not a decision: bad input shapes are refused.
- `credit_state(readings)` — Reduce ``dunkingsheep usage --json`` output to {provider: state}.
- `tier_headroom(credit, tier)` — Lowest FINITE remaining percentage for a tier, or None.
- `tier_credit(credit, tier)` — The lowest remaining percentage reported for a tier, or None.
- `tier_state(credit, tier)` — Availability of one tier, from the reduced credit state.
- `_provider_label(tier)`
- `_credit_label(credit, tier)`
- `_reason_for_skip(reason, **extra)`
- `plan_swap(pane, credit, allowed_panes)` — Decide whether ``pane`` should be relaunched on another tier.
- `plan_all(panes, credit, allowed_panes)` — Plan every pane, in stable pane-id order so two readers agree.

## dunk_github.py (611 lines)

> GitHub Apps for the flat fleet: two bot identities, tokens minted on demand.  `docs/FLAT_FLEET.md` section 6. The owner has one GitHub login and no

- const `CONFIG_DIR` = `Path(os.environ.get("DUNKINGSHEEP_DIR", str(Path.home() / ".config" / "dunkingsheep")))`
- const `API` = `os.environ.get("DUNKINGSHEEP_GITHUB_API", "https://api.github.com")`
- const `KINDS` = `("author", "reviewer")`
- const `DEFAULT_REPOS` = `("huntergdavis/dunkingsheep", "huntergdavis/poolrad-decomp")`
- const `DEFAULT_REQUIRED_VERDICTS` = `1`
- const `REPO_REQUIRED_VERDICTS` = `{"huntergdavis/poolrad-decomp": 2}`
- const `REVIEW_CONTEXT` = `"dunkingsheep/review"`
- const `REFRESH_MARGIN_S` = `10 * 60`
- class **GitHubError** — A named refusal or API failure.
- `_jwt_module()`
- `app_file(kind)`
- `load_app(kind)` — {"id", "slug", "pem"} for an app kind, or raise naming what is missing.
- `app_jwt(app, now)`
- `_request(method, url, token, bearer, payload, timeout)`
- class **Tokens** — Installation-token cache, one per app kind, refreshed before expiry.
  - `__init__(path)`
  - `_load()`
  - `_save()`
  - `installation_id(app, bearer)`
  - `get(kind, now)` — A live installation token for `kind`: {token, expires_at, kind, slug}.
  - `forget(kind)`
- const `ROLE_KIND` = `{"worker": "author", "planner": None, "desk": None, "reviewer": "reviewer", "system": "rev`
- `kind_for_role(role, requested)` — Which app a role may act as.
- `open_pulls(repo, token)`
- `pull_detail(repo, number, token)` — One PR, with the two fields that say whether it can merge (t704).
- `post_status(repo, sha, state, token, description, target_url)` — Commit status `dunkingsheep/review` on a head, as the reviewer App.
- `merge_pull(repo, number, token, sha)` — Merge an accepted PR as the reviewer App once branch protection is satisfied.
- `review_pull(repo, number, event, body, token)` — Approve or request changes on a PR as the reviewer App.
- const `GRAPHQL` = `API.rstrip("/") + "/graphql"`
- `graphql(query, token, variables)` — A GraphQL call; a 200 response carrying `errors` is a failure, not a result.
- `get_ref(repo, ref, token)` — The commit sha a branch points at right now.
- `ref_exists(repo, branch, token)`
- `commit_tree(repo, sha, token)` — The tree sha of a commit.
- `file_text(repo, path, ref, token)` — (text, blob_sha) for one file at `ref`; (None, None) when it is not there.
- `pr_files(repo, number, token)` — Every file a PR changes: [{filename, status, previous_filename}].
- `create_tree(repo, base_tree, entries, token)` — A tree on top of `base_tree`; entries are git tree entries (path, mode, content).
- `create_commit(repo, message, tree, parents, token)`
- `create_ref(repo, branch, sha, token)` — Create `refs/heads/<branch>` at `sha`.
- `open_pull(repo, title, head, base, body, token)` — Open a PR; {number, url, node_id, head}.
- `pulls_for_head(repo, branch, token)` — Open PRs whose head branch is `branch` (owner: the repo, so no org filter).
- `enable_auto_merge(repo, number, token, method)` — Queue the PR for auto-merge as soon as branch protection is satisfied.
- `disable_auto_merge(repo, number, token)`
- `pull_state(repo, number, token)` — {state, merged, mergeable_state, review_decision, auto_merge} for a PR.
- `protection_payload(approvals, reviewer_app_id, bypass_app_slug)` — The branch-protection body for `main` (FLAT_FLEET.md 6.2).
- `required_verdicts(repo, config)` — How many DISTINCT reviewer lanes a head needs before it is final (t662).
- `repos_config()`
- const `AUTHOR_MINT_FILE` = `"github-author-mints.json"`
- const `AUTHOR_MINT_WINDOW_S` = `1800`
- const `AUTHOR_MINT_KEEP_S` = `86400`
- `author_mint_path(path)`
- `record_author_mint(pane, at, path)` — Remember that `pane` minted the AUTHOR App token at `at` (t723).
- `author_from_mints(since, path, now)` — The ONE pane that minted the author token since `since`, else None (t723).

## dunk_guard.py (356 lines)

> The herdr guard: a `herdr` wrapper that routes typing through Dunking Sheep.  Dunking Sheep never types over someone who is mid-sentence, but that only

- const `CONFIG_DIR` = `os.environ.get("DUNKINGSHEEP_DIR") or os.path.expanduser( "~/.config/dunkingsheep" )`
- const `GUARD_DIR` = `os.path.join(CONFIG_DIR, "bin")`
- const `GUARD_PATH` = `os.path.join(GUARD_DIR, "herdr")`
- const `TMUX_GUARD_DIR` = `os.path.join(CONFIG_DIR, "tmux-guard")`
- const `TMUX_WRAPPER_PATH` = `os.path.join(os.path.dirname(os.path.abspath(__file__)), "dunk_tmux_guard.py")`
- const `BASHRC` = `os.path.expanduser("~/.bashrc")`
- const `MARKER` = `"# dunkingsheep herdr guard"`
- const `GUARD_MARKER` = `"dunkingsheep-herdr-guard"`
- const `REAL_SUFFIX` = `"-real"`
- const `WRAPPER_PATH` = `os.path.join(os.path.dirname(os.path.abspath(__file__)), "dunk_guard_wrapper.py")`
- `_wrapper()` — The guard source, with the CLI path substituted.
- const `PATH_LINE` = `'export PATH="{guard_dir}:$PATH"'`
- const `LAUNCHER` = `"""#!/bin/sh # Dunking Sheep launcher, installed by `dunkingsheep install`. # Keeps the CL`
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
- `check()` — Prove the guard intercepts: run the wrapper with a harmless command that would type, against a pane id that does not exist, and confirm it went to the daemon rather than to herdr.

## dunk_guard_wrapper.py (182 lines)

> herdr guard - installed by `dunkingsheep guard install`; do not edit.  Routes pane-typing and destructive subcommands through the Dunking Sheep daemon

- const `DUNKINGSHEEP` = `__DUNKINGSHEEP__`
- const `GUARD_DIR` = `os.path.dirname(os.path.realpath(__file__))`
- const `NEWLINE` = `chr(10)`
- `is_guard(path)`
- `real_herdr()` — The actual herdr binary, never this wrapper.
- `passthrough(argv)`
- `daemon_json(args, timeout)`
- `report_failure(text)`
- `route_close(kind, target)`
- `route_herdr(argv)`
- `route(kind, target, payload, argv)` — Hand a typing command to the daemon.
- const `MUTATING` = `( ("pane", "split"), ("pane", "rename"), ("pane", "focus"), ("tab", "create"), ("tab", "re`
- const `CLOSING` = `( ("pane", "close", "pane"), ("pane", "kill", "pane"), ("tab", "close", "tab"), ("workspac`
- `main(argv)`

## dunk_health.py (202 lines)

> Deterministic self-check and self-repair for the Dunking Sheep daemon.  There is no model in this loop. Every check has a fixed threshold and, when

- const `WAL_WARN_BYTES` = `64 * 1024 * 1024`
- const `DISPATCH_STALE_S` = `16 * 60`
- const `DISK_MIN_FREE_BYTES` = `512 * 1024 * 1024`
- const `HEALTH_PATH` = `os.path.expanduser("~/.config/dunkingsheep/health.json")`
- `_stale_leases(path, now)` — Count ingest rows a dead worker left behind.
- `_quick_check(path)`
- `_dispatch_freshness(config_dir, now)` — How long since a dispatcher tick completed: 'the feeder is dead' as a number.
- `run_health(flock, repair, deep)`
- `write_report(result, path)` — Write the last report where ops tooling (and the VP) can read it.

## dunk_help.py (759 lines)

> The one guide to Dunking Sheep, shared by every surface.  - `dunkingsheep guide` prints it (plain text or `--markdown`).

- const `SHORT` = `"""\ Dunking Sheep types text (plus Enter) into terminal panes on a schedule. Each schedul`
- `guide(markdown)` — Return the full guide as plain text (default) or GitHub markdown.
- `_underline(text, level)`

## dunk_http.py (290 lines)

> HTTP surface for the Command Center.  The UI needs a few shapes (roster, message state, reply stats, project summary,

- const `DEFAULT_PORT` = `int(os.environ.get("DUNKINGSHEEP_API_PORT", "8792"))`
- const `DEFAULT_HOST` = `os.environ.get("DUNKINGSHEEP_API_HOST", "127.0.0.1")`
- const `MAX_POST_BYTES` = `4096`
- class **ApiError** — An answer that is not a success, carrying what the caller needs to act on it.
  - `__init__(status, message, context)`
- `error_body(error, message)` — The JSON a client sees for a refusal: error first, then only echoed context.
- `_client()`
- `_call(command, **args)`
- `_unique_names(panes)` — Give every pane a name a caller can address it by: the agent label, made unique.
- `_lane_states()` — Pane status from the daemon, plus the reason when it cannot be had.
- `terminals()` — The tmux terminal list, read from the backend this host actually runs.
- `route(path, query)`
- `route_post(path, payload)` — The write half.
- `resize_terminal(name, payload)`
- `_now()`
- class **Handler**
  - `do_GET()`
  - `do_POST()`
  - `log_message(fmt, *args)`
- `serve(host, port)`
- `main(argv)`

## dunk_inbox.py (1056 lines)

> Per-agent inboxes: machine-to-machine messaging that never types.  Everything else in Dunking Sheep delivers by typing into a terminal's input

- const `CONFIG_DIR` = `os.environ.get("DUNKINGSHEEP_DIR") or os.path.expanduser( "~/.config/dunkingsheep" )`
- const `INBOX_DB` = `os.path.join(CONFIG_DIR, "inbox.db")`
- const `BLOB_DIR` = `os.path.join(CONFIG_DIR, "attachments")`
- const `RETENTION_S` = `7 * 24 * 3600`
- const `MAX_ATTACHMENT_BYTES` = `64 * 1024 * 1024`
- const `MAX_BLOB_BYTES` = `2 * 1024 * 1024 * 1024`
- const `MAX_BODY_BYTES` = `8 * 1024 * 1024`
- const `STATES` = `("queued", "read", "acked", "cancelled", "archived", "undeliverable")`
- const `OWNER_MAILBOX` = `"owner"`
- const `TRANSPORTS` = `("web", "telegram", "report")`
- `validate_transport(transport)` — None (a lane message) or one of TRANSPORTS.
- const `PANE_MAILBOX_RE` = `re.compile(r"^(?:w\d+:p\d+|%\d+)$")`
- const `SCHEMA` = `""" CREATE TABLE IF NOT EXISTS messages ( id TEXT PRIMARY KEY, mailbox TEXT NOT NULL, mail`
- const `_PANE_SUFFIX` = `re.compile(r"\s*\(\s*(?:[A-Za-z0-9_.-]+:p[A-Za-z0-9_.-]+|%\d+)\s*\)\s*$")`
- `normalise_mailbox(name)` — 'vp of agentry (w9:pH)' and 'vp of agentry' are ONE mailbox.
- `mailbox_where(mailbox, column)` — SQL that matches either spelling the table may hold for one mailbox.
- class **InboxError** — A caller mistake (unknown id, missing file, too big).
- `_now()`
- class **Inbox** — The message store.
  - `__init__(path, blob_dir, retention_s)`
  - `close()`
  - `_next_id()` — Message ids are stable, never reused, and survive restarts: the counter lives in the database beside the messages.
  - `ensure_next_job(message_id, mailbox, pane_id, body, subject)` — Materialise a task-store outbox intent once, including after a crash.
  - `send(mailbox, body, subject, sender, mailbox_label, pane_id, attachments, transport, reply_to, external_id)` — Put a message in a mailbox.
  - `upsert_digest(mailbox, digest_id, body, subject, sender, mailbox_label, pane_id)` — One message per digest id, updated in place.
  - `rename_mailbox(old, new)` — Move every message from one mailbox key to another.
  - `record_notify(message_id, state, reason, transport_id, attempt)`
  - `count(mailbox, lane_panes)`
  - `list(mailbox, unread_only, limit)` — Summaries, oldest first.
  - `replies_to(message_id)` — Rows answering `message_id`, oldest first.
  - `conversation(mailbox, limit, before)` — The mailbox as a conversation: questions and replies, oldest first.
  - `read(mailbox, limit, peek, mark)` — Full messages in queue order.
  - `mark_read(message_ids, now)` — Commit the read for ids a caller has already rendered.
  - `record_delivery(mailbox, message_ids, now)` — Remember the latest delivered batch for a mailbox: the recovery buffer.
  - `pending_delivery(mailbox)` — The receipt for a mailbox, or None when nothing is recoverable.
  - `recover_delivery(mailbox)` — Replay the receipt batch for display, whatever its read state.
  - `note_acked(mailbox)` — Shrink the receipt to ids that are still not acked.
  - `find_by_external_id(mailbox, external_id)` — The row a transport's own id already produced, or None (t594).
  - `owner_outbox(transport, limit)` — Owner-mailbox REPLIES waiting to go out on `transport` (t594).
  - `mark_delivered(message_id, transport, external_id)` — Record that a transport accepted this row: the delivery RECEIPT (t594).
  - `get(message_id, with_body)`
  - `status(message_id)` — State of one message, by id, with no message text in it.
  - `stats()`
  - `ack(message_ids, mailbox)`
  - `cancel(message_ids)`
  - `archive(mailbox, before, states)` — Bulk-retire a mailbox without deleting anything: mark messages read and archived (counted separately from unread).
  - `mark_undeliverable(live_panes)` — A queued message whose resolved pane id no longer exists is undeliverable, not silently queued.
  - `auto_archive_undeliverable(age_s, now)` — Archive undeliverable mail whose target has been gone for `age_s`.
  - `archive_retired(mailboxes, age_s, now, reason, authorize, identity_guard)` — Archive mail addressed to a RETIRED pane's mailbox, keeping every row.
  - `_transition(message_ids, state, stamp, allowed, read_stamp, mailbox)`
  - `delete(message_ids)`
  - `purge(older_than_s)` — Drop read, acknowledged and cancelled mail past its retention.
  - `_store_blob(path)`
  - `fetch(attachment_id, dest_dir)` — Copy an attachment out of the store.
  - `_blob_bytes()`
  - `_collect_blobs()` — Drop blobs no message references any more.
  - `_attachments(message_id)`
  - `_summary(row)`
  - `_full(row)`
- `_media_type(path)`

## dunk_load.py (254 lines)

> Per-caller CPU from /proc, with a pid+starttime guard and process trees.  Identity is the registered caller (pid + starttime). A recycled pid produces no

- const `TICKS` = `os.sysconf("SC_CLK_TCK") if hasattr(os, "sysconf") else 100`
- const `STATE` = `Path(os.environ.get( "DUNKINGSHEEP_LOAD_STATE", Path(os.environ.get("DUNKINGSHEEP_DIR", st`
- class **LoadError** — A named refusal for a bad sampler input.
- `_stat(pid)` — Fields of /proc/<pid>/stat we need, or None when the process is gone.
- `_ppid_map()` — One walk: pid -> ppid for every visible process.
- `tree_of(pid, ppids)` — The caller plus descendants, via the ppid map.
- `tree_cpu(pid, ppids)` — Summed utime+stime and RSS for the tree, or None if the root is gone.
- `registered_callers(tasks)` — [(name, pid, starttime)] from task leases that carry caller identity.
- `sample(callers, now)` — One sample: per caller, CPU ticks now and the pid-reuse verdict.
- `gpu()` — Per-process GPU here is a NULL with a reason, never a fabricated number.
- `_load_state(path)`
- `_save_state(state, path)`
- `collect(callers, now, policies, state_path)` — Sample and turn ticks into a window percentage of one core.

## dunk_mcp.py (306 lines)

> Dunking Sheep MCP server (stdio transport, standard library only).  Speaks JSON-RPC 2.0 over newline-delimited stdin/stdout, as the Model Context

- const `SUPPORTED_PROTOCOLS` = `("2025-11-25", "2025-06-18", "2025-03-26", "2024-11-05")`
- const `STRUCTURED_CONTENT_SINCE` = `"2025-06-18"`
- const `INSTRUCTIONS` = `"""\ Dunking Sheep sends text (plus Enter) into herdr terminal panes on a schedule. Each s`
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

- const `CONFIG_DIR` = `os.environ.get("DUNKINGSHEEP_DIR") or os.path.expanduser( "~/.config/dunkingsheep" )`
- const `MEMORY_DB` = `os.path.join(CONFIG_DIR, "memory.db")`
- const `SCHEMA_VERSION` = `1`
- const `DEFAULT_CHUNK_CHARS` = `4000`
- const `DEFAULT_OVERLAP_CHARS` = `400`
- const `SCHEMA` = `""" PRAGMA journal_mode = WAL; PRAGMA synchronous = NORMAL; PRAGMA foreign_keys = ON; PRAG`
- const `SECRET_PATTERNS` = `( (re.compile(r"-----BEGIN [A-Z0-9 ]*PRIVATE KEY-----.*?-----END [A-Z0-9 ]*PRIVATE KEY----`
- `redact(text)` — Replace credential-shaped substrings with a stable marker.
- `chunk_text(text, max_chars, overlap_chars)` — Split text into overlapping chunks at whitespace boundaries.
- `fts_tokens(text)`
- `fts_query(text, operator)` — Safe FTS5 query: quoted tokens joined by operator (AND or OR).
- `make_snippet(text, tokens, max_chars)` — Cheap snippet from the stored text; do not use FTS5 snippet() in the query, which scans every match.
- `memory_sources()` — Known session sources, most recently modified first.
- `_harness_from_path(path)`
- `_project_from_cwd(cwd)`
- `_text_from_content(content)` — Extract text from a Claude/Codex/pi content value.
- class **MemoryStore** — The database.
  - `__init__(path, create)`
  - `_new_read_connection()`
  - `_read_connection()` — Reuse a pooled read-only connection; opening one per query serialises on the WAL shm lock under 20 concurrent clients.
  - `_release_connection(conn)`
  - `close()`
  - `_begin_write()`
  - `_record_query(latency_ms, hits, error, cached)`
  - `_percentiles()`
  - `enqueue(path, harness, project)`
  - `checkpoint()` — Best-effort WAL checkpoint.
  - `requeue_partial()` — Resume sources left partial by a deliberate stop (daemon restart).
  - `recover_leases()` — Requeue ingest leases whose worker died (expired lease).
  - `claim_next(lease_s)`
  - `finish(lease, ok, error, stats, skipped)`
  - `ingest_path(path, harness, project, should_stop, throttle_s)` — Parse one source.
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
- class **MemoryIngestor** — The single writer.
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

- const `CONFIG_DIR` = `os.environ.get("DUNKINGSHEEP_DIR") or os.path.expanduser( "~/.config/dunkingsheep" )`
- const `METRICS_DB` = `os.path.join(CONFIG_DIR, "metrics.db")`
- const `RETENTION_S` = `30 * 24 * 3600`
- const `MAX_EVENTS` = `2_000_000`
- const `SCHEMA_VERSION` = `1`
- const `SCHEMA` = `""" CREATE TABLE IF NOT EXISTS events ( id INTEGER PRIMARY KEY AUTOINCREMENT, at REAL NOT `
- const `KINDS` = `{ "message.sent": "text was typed into a pane and confirmed submitted", "message.failed": `
- const `OUTCOMES` = `{ "ok": "completed as intended", "failed": "attempted and did not complete", "held": "not `
- const `REASONS` = `{ "draft": "someone was writing in the input box", "dialog": "a dialog was open, waiting f`
- class **Metrics** — Append-only event store plus the aggregation the endpoint returns.
  - `__init__(path, retention_s, enabled)`
  - `close()`
  - `record(kind, pane_id, mailbox, sender, source, outcome, reason, duration_ms, attempts, bytes, at)` — Append one event.
  - `prune(older_than_s, max_events)`
  - `earliest()`
  - `_rows(start, end)`
  - `summary(start, end)` — Counts and duration distributions for one window.
- `_tally(rows, column)`
- `_histogram(values)`
- `_distribution(rows, kind)`
- `_percentile(sorted_values, fraction)` — Nearest-rank percentile.

## dunk_native_session.py (327 lines)

> Fail-closed native-session activity guard for same-session Codex resume.  The guarded same-pane respawn replaces a pane's process; it does not stop a

- class **NativeSessionRefusal** — The native session cannot be proven quiescent; callers must refuse.
- class **NativeSessionChanged** — The rollout changed after the quiescence snapshot was taken.
- class **NativeSessionBusy** — The private state DB is busy; the quiescence check can be retried.
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

- class **DispatchPaused** — The operator withdrew dispatch permission; retain the durable intent.
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

- const `CONFIG_DIR` = `os.environ.get("DUNKINGSHEEP_DIR", os.path.expanduser("~/.config/dunkingsheep"))`
- const `DB_PATH` = `os.path.join(CONFIG_DIR, "owner-push.db")`
- const `SCHEMA` = `""" CREATE TABLE IF NOT EXISTS owner_push ( id INTEGER PRIMARY KEY AUTOINCREMENT, pane_id `
- const `_SHARED` = `{}`
- const `_SHARED_LOCK` = `threading.Lock()`
- `shared(path)` — ONE connection for the daemon, behind the store's own lock.
- class **OwnerPush**
  - `__init__(path)`
  - `__enter__()`
  - `__exit__(*exc_info)`
  - `close()` — Close the connection.
  - `push(pane_id, body, target, subject, now)`
  - `receipts(push_ids)` — READ-ONLY arrival receipts for specific pushes (t66, %33's request).
  - `take(pane_id, by, limit, now)` — Hand over what is waiting for this pane, oldest first.
  - `poll(pane_id, by, now)` — Record a successful poll by the lane's own harness (empty polls count).
  - `last_poll(pane_id)` — When this pane's harness last polled, or None.
  - `last_take(pane_id)` — When this pane last drained a push, or None.
  - `pending(pane_id)`
  - `stats(window_s, now)`
- `_pct(values, q)`

## dunk_panes.py (201 lines)

> Retired panes: the ones we deliberately stopped feeding.  A pane can be superseded (a replacement was started elsewhere) or replaced by

- const `CONFIG_DIR` = `os.environ.get("DUNKINGSHEEP_DIR", os.path.expanduser("~/.config/dunkingsheep"))`
- const `PATH` = `os.path.join(CONFIG_DIR, "retired-panes.json")`
- const `PANE_IN_MAILBOX` = `re.compile(r"([A-Za-z0-9_.-]+:p[A-Za-z0-9_.-]+)")`
- class **RetireError** — A named refusal.
- `_load(path)`
- `identity_guard(paths, exclusive)` — Coordinate managed identity writers with a guarded archival transaction.
- `_write(data, path)`
- `_write_locked(data, path)`
- `retire(pane_id, reason, by, labels, mailboxes, path, now)` — Mark a pane retired.
- `unretire(pane_id, path)`
- `all_retired(path)`
- `is_retired(pane_id, path)`
- `mailbox_retired(mailbox, path)` — True when a mailbox belongs to a retired pane, however it is spelled.
- const `_ARCHIVE_PANE` = `r"(?:[A-Za-z0-9_.-]+:p[A-Za-z0-9_.-]+|%\d+)"`
- const `_ARCHIVE_SUFFIX` = `re.compile(r"\s*\((" + _ARCHIVE_PANE + r")\)\s*$")`
- `valid_retirements(records)` — A reasonless or undated record cannot withdraw identity protection.
- `retirement_aliases(record)`
- `retired_mailboxes(candidates, records, live_panes, protected)` — Exact database keys with positive retirement evidence, safe to age-archive.

## dunk_pm.py (170 lines)

> Project PMs: lane->PM routing and the one-per-project digest.  A PM does not get a stream of reports. It publishes ONE digest per project per

- const `DEFAULT_VP_PANE` = `os.environ.get("DUNKINGSHEEP_VP_PANE", "w9:pH")`
- const `ROUTING` = `Path(os.environ.get( "DUNKINGSHEEP_PM_ROUTING", Path.home() / ".config" / "dunkingsheep" /`
- class **PmError** — A named refusal for a bad routing file or digest payload.
- `load_routing(path, vp_pane)` — Return {project: {pm, members, mode, vp_pane}}.
- `project_for(pane_id, routing)` — The project whose member list contains this pane, if any.
- `redirect(pane_id, routing, vp_pane)` — Decide where a lane's message to the VP should actually go.
- const `POOLRAD_CHECKPOINT` = `Path(os.environ.get( "POOLRAD_CHECKPOINT", "~/workspace/poolrad-decomp/docs/re/function_ac`
- `checkpoint_numbers(project, path)` — Read the project's numbers AT PUBLISH TIME.
- `render_digest(project, changed, blocked, owner_decisions, numbers, window, note)` — Render the four-section digest.
- `digest_id(project)`
- `parse_payload(raw)` — Accept a JSON digest payload or a plain body string.

## dunk_policy.py (149 lines)

> Per-agent resource policy: alerts, conservative auto-kill, and a deny list.  Measurement comes from the #1252 sampler (dunk_load), so this module never

- const `DEFAULT_GRACE_S` = `30.0`
- class **PolicyError** — A named refusal for a malformed policy or sample.
- `_num(policy, key, default)`
- `evaluate(sample, policy, now)` — Findings for one caller sample against one agent's policy.
- `denied(pid, deny_pids, deny_panes, workspace_roots, cwd, protected_panes)` — The deny evaluation.
- `kill_decision(sample, policy, registered, deny_pids, deny_panes, workspace_roots, protected_panes, dry_run, grace_s, signal_fn, sleep_fn, now)` — Decide and (unless dry_run) act on one over-threshold caller.

## dunk_poolrad.py (615 lines)

> Poolrad backlog-sync and progress (FLAT_FLEET.md 6.5): the queue fills itself.  The VP and CoS used to write every claimable Poolrad body by hand. This is the

- const `CONFIG_DIR` = `Path(os.environ.get("DUNKINGSHEEP_DIR", str(Path.home() / ".config" / "dunkingsheep")))`
- const `DEFAULT_REPO` = `os.environ.get("DUNKINGSHEEP_POOLRAD_REPO", "/home/hunter/workspace/poolrad-decomp")`
- const `CEILING` = `815`
- const `QUEUE_JSON` = `CONFIG_DIR / "poolrad" / "strict-queue.json"`
- const `EXPORT_ARGS` = `[ "--inventory", "resources/function-accounting-current/inventory.json", "--callgraph", "r`
- const `STRICT_MANIFEST` = `"docs/re/strict_full_c_addresses.csv"`
- const `PARTIAL_MODELS` = `"docs/re/partial_callee_models.csv"`
- const `IDENTITIES` = `"docs/re/reviewed_function_identities.csv"`
- const `METHOD_DOC` = `"docs/re/STRICT_CANDIDATE_QUEUE.md"`
- const `TITLE_PREFIX` = `"Poolrad strict C:"`
- `_git(repo, *args)`
- `credited_ids(repo)`
- `identity_count(repo)`
- `export_queue(repo, out, timeout)` — Run the repo's exporter read-only; the JSON lands in our state dir.
- `candidates(queue, credited, filed_ids, limit, closed)` — Ranked candidates worth filing: composable, not credited, not filed.
- `task_title(row)`
- `task_body(row, base, repo)`
- const `EXPORTER_INLINE_IN_STRICT` = `"inline_in_strict"`
- const `EXPORTER_MAIN_MODEL` = `"main_model"`
- const `FILED_KEY_PREFIX` = `"poolrad-strict:"`
- const `FILED_KEY` = `re.compile(r"^filed-key: " + re.escape(FILED_KEY_PREFIX) + r"(?P<id>\S+?)@(?P<base>\S+)\s*`
- `filed_keys_from_tasks(tasks)` — {row id: (task id, status)} for every task that already filed this row.
- `filed_ids_from_tasks(tasks)` — The row ids already filed, in any status (kept for callers that want ids).
- `disposition_closures(repo)` — Row ids a reviewed DISPOSITION document on main has already closed (t686).
- `registered_checker_ids(repo)` — Row ids a registered checker in tools/overlay already covers (t686).
- `_unit_tokens(rid)` — 'OVL_006A_0824' -> {'006a_0824', '6a_0824'} - the shapes files use.
- `_merge_pr(repo, commit)` — The PR number that carried `commit`, from the merge commit's subject (t733).
- `_model_name_matches(name, rid)` — True when a recovered-model filename carries this row's unit AND offset (t733).
- `model_evidence(repo, rid)` — Where this row's model ALREADY lives on main: (model, commit, pr) or None (t733).
- `credit_pending_path(path)`
- `record_credit_pending(entries, path)` — Merge `entries` ({id: evidence}) into the credit-pending list (t733).
- `credit_pending(path)` — The recorded credit-pending bodies, {id: evidence} (t733).
- `model_statuses(repo)` — Row id -> status from docs/re/partial_callee_models.csv (t733/t738).
- `credit_ready(repo, rid, checkers, statuses)` — (ready, why_not) for calling a row's model "on main, credit pending" (t738).
- `already_on_main(repo, rid, checkers)` — The model source + a registered checker for this row are both on main (t686).
- class **Sync**
  - `__init__(flock, repo, exporter, credit_path)`
  - `run(limit, dry_run, priority)`
- `progress(repo, queue_path, credit_path)`

## dunk_recovery.py (136 lines)

> t504: bounded, deterministic recovery for a DS-owned prompt in an ambiguous frame.  The measured stall: d82 typed its own prompt into %2 at 08:28 and the lane never

- const `RECOVERABLE_ATTEMPT_STATES` = `frozenset({"pending", "unconfirmed", "typed"})`
- `_norm(text)` — Compare what a person sees, not what a terminal wrapped.
- `_refuse(reason, **extra)`
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

## dunk_report.py (416 lines)

> The reporter: a deterministic status report, no model (FLAT_FLEET.md 7.3).  The VP used to write this by hand, which is why the fleet spent a week building

- const `CONFIG_DIR` = `os.environ.get("DUNKINGSHEEP_DIR") or os.path.expanduser( "~/.config/dunkingsheep")`
- const `REPORT_DIR` = `os.path.join(CONFIG_DIR, "reports")`
- const `OWNER_MAILBOX` = `"owner"`
- const `REPORT_TRANSPORT` = `"report"`
- const `WINDOW_S` = `24 * 3600`
- const `TASK_ID` = `re.compile(r"\bt\d{1,5}\b")`
- const `STATUSES` = `("open", "in_progress", "blocked", "done", "cancelled")`
- `_short(path)`
- `_iso(at)`
- `_age(seconds)`
- `task_section(flock, now)`
- `lanes_section(flock, now)` — Working / idle / dead, from live panes and the leases they hold.
- `needs_owner_section(flock, now)`
- `review_section(flock, now)`
- `poolrad_section(flock, now)`
- `budget_section(now)` — Provider budget, when the bridges' usage cache can be read.
- `watchdog_section(now)`
- `collect(flock, now)`
- `_counts_line(mapping, order)`
- `render(report)` — The deterministic block.
- `prose_problems(prose, report)` — Why `prose` may not be posted, or an empty list.
- `write(report, text, out_dir)`
- `list_reports(out_dir)`
- `run(flock, now, out_dir, post, prose, post_fn)` — Generate, write, and (when the owner mailbox exists) post one report.

## dunk_review.py (878 lines)

> The review timer: GitHub PRs -> review tasks -> commit status (FLAT_FLEET.md 6.3).  Deterministic, no model. Every two minutes:

- const `ACCOUNTING_LEDGERS` = `{ "huntergdavis/poolrad-decomp": { "ledger": "docs/re/strict_full_c_addresses.csv", "base"`
- const `ROW_ID` = `re.compile(r"^(?P<kind>OVL|RES)_(?P<unit>[0-9A-F]{4})_(?P<off>[0-9A-F]{4})$")`
- const `ROW_TOKEN` = `re.compile( r"(?<![A-Za-z0-9_])(?:OVL|RES)_[0-9A-F]{4}_[0-9A-F]{4}(?![A-Za-z0-9_])")`
- `attribute_author(pr, lanes, mints)` — Pane id of the lane that opened `pr`, or None.
- `author_from_tasks(repo, number, flock)` — The lane that worked on `repo#number`, when exactly ONE did (t723).
- `_iso(value)`
- `row_ids(text)` — Every full row id named in `text`, in order of first appearance, deduped.
- `stem_of(path)`
- `own_row(row, stem)` — True when a row id is this document's by name: the stem carries the offset as an underscore token and the unit as `u<unit>` or the four-digit unit (poolrad convention: U7F_STATE_FLAGS_0F7E.md <- OVL_007F_0F7E).
- `ledger_rows(text)` — (id, evidence_doc) pairs from a `id,evidence_doc` ledger, header skipped.
- `with_row(text, ident, doc)` — The ledger with one row inserted in sorted position.
- `choose_row(sources, added_docs, ledger_text)` — (row, reason): the one ledger row an accepted body earns, or why there is none.
- `review_body(pr, detail, author)`
- class **Sync**
  - `__init__(flock, tokens, github)`
  - `_live_reviews()`
  - `_judged_heads()` — (repo, pr, head) -> verdict for every DONE review: a head that was judged is never re-filed (2026-10-29: an accepted PR got a second review task the next tick, whose pending status overwrote the success).
  - `head_verdicts(repo, head, number)` — The verdicts recorded at one head, by DISTINCT lane (t662).
  - `_is_final(repo, head, number, lane)` — Is this head final: enough DISTINCT accepting lanes and no needs-fix?
  - `_second_review(repo, number, head, first_lane, report)` — File the second review for a head whose first verdict was an accept (t662).
  - `conflicting(repo, number, token, report)` — True / False when GitHub says, None when it will not say (t704).
  - `run(dry_run)`
  - `_account_after_merge(repo, number, head, task_id, report)` — Open the ledger row for a body that just merged (t835), once.
  - `_merge(repo, number, head, token, report, task_id)`
  - `_status(repo, sha, state, token, description, url, report)`
  - `on_verdict(task, verdict, by)` — Called by the daemon when a review completes: status + approval + follow-up.
  - `_verdict_text(task)` — The reviewer's own words: every note on the review task, oldest first.
  - `account(task, by, dry_run)` — Open, approve and queue the accounting PR for an accepted poolrad body.
  - `_accounting_body(task, ident, doc, ledger, number, by)`
  - `_record_accounting(task, out)` — Put both PR numbers on the review task, and the reason when there is no PR.
  - `_standing_accept(task)` — The task id of another DONE review that accepted this exact head, or None.
  - `_evidence(task)` — The reviewer's own notes, verbatim, or a sentence saying there are none.
  - `_review_body(task, by, head, evidence)`

## dunk_roles.py (432 lines)

> Roles: who a caller is, bound to a process, and what that role may call.  Replaces the superuser allowlist (`dunk_superusers.py`) and the desk credential

- const `CONFIG_DIR` = `Path(os.environ.get("DUNKINGSHEEP_DIR", str(Path.home() / ".config" / "dunkingsheep")))`
- const `ROLES_FILE` = `str(CONFIG_DIR / "roles.json")`
- const `TOKENS_FILE` = `str(CONFIG_DIR / "service-tokens.json")`
- const `ROLES` = `("owner", "system", "web", "desk", "planner", "worker", "reviewer", "anonymous")`
- const `LANE_ROLES` = `("desk", "planner", "worker", "reviewer", "owner")`
- const `TOKEN_ROLES` = `("web", "system", "owner")`
- const `DEFAULT_ROLE` = `"anonymous"`
- const `UNCONFIGURED_DEFAULT` = `"worker"`
- const `DEFAULT_REVIEWER_PROVIDERS` = `("claude", "codex")`
- const `READ_ONLY` = `frozenset({ "status", "poll", "health", "help", "doctor", "whoami", "roster", "message_sta`
- const `_WORKER` = `READ_ONLY | frozenset({ "task_next", "task_heartbeat", "task_release", "task_complete", "t`
- const `ALLOW` = `{ "owner": None, # everything "system": READ_ONLY | frozenset({ "task_add", "task_assign",`
- const `WEB_DENIED` = `frozenset({ "close", "shutdown", "respawn_pane", "start_agent", "create_workspace", "creat`
- const `DESK_LIMIT` = `5`
- const `DESK_RESULT_BYTES` = `4096`
- class **RoleError** — A named refusal.
- `allowed(role, command)` — May `role` call `command`?
- `_hash(token)`
- `_read_json(path)`
- `_write_json(path, payload)`
- `_stamp(path)`
- class **Roles** — The lane registry plus the service-token table.
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

## dunk_secrets.py (167 lines)

> Refuse to publish the desk's own secrets (t756).  On 2026-10-29 two commits on two public PRs added `data/` to this repo:

- const `SECRET_PATTERNS` = `( "data/", "data", "*.sqlite3", "*-credentials", "owner-credentials", "*.pem", "*token*", `
- `matches(path)` — The pattern that makes `path` unpublishable, or None.
- `leaks(paths)` — [(path, pattern)] for every path that must not be published.
- `_git(args, cwd)`
- `added_paths(cwd, base)` — Paths this branch ADDS relative to `base`, plus anything staged.
- `refusal(cwd, base)` — (message, error) - the named refusal, or (None, error-or-None).
- `main(argv)` — CLI: `dunkingsheep secrets-check [--base REF]` - exit 1 on a refusal.

## dunk_server.py (410 lines)

> Dunking Sheep daemon: owns the `Flock` and serves it over a unix socket.  Protocol (newline-delimited JSON, any number of requests per connection):

- const `DEFAULT_SOCKET` = `os.path.join(CONFIG_DIR, "dunkingsheep.sock")`
- const `SOCKET_PATH` = `os.environ.get("DUNKINGSHEEP_SOCKET") or DEFAULT_SOCKET`
- const `LOG_FILE` = `os.path.join(CONFIG_DIR, "daemon.log")`
- class **AlreadyRunning**
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
- `configure_logging(to_file, level)` — Log to the daemon log file, plus stderr when it is a terminal (a detached daemon has stderr redirected into the same file, so skip it then).
- `kill_own_process_group(reason)` — SIGTERM the daemon's whole process group (t212).
- `run_daemon(socket_path, state_file, log_to_file)` — Blocking entry point used by `dunkingsheep serve`.

## dunk_sqlite.py (30 lines)

> Consistent SQLite connections for daemon stores and read-only consumers.

- const `BUSY_TIMEOUT_MS` = `5000`
- `connect(path, readonly, check_same_thread, timeout)` — Use WAL for owned stores; never change journal mode on external reads.

## dunk_state.py (602 lines)

> Continuous agent-state capture: what it would take to put a lane back.  The owner asked for every agent to be restartable by the VP with its previous state,

- const `CONFIG_DIR` = `os.environ.get("DUNKINGSHEEP_DIR", os.path.expanduser("~/.config/dunkingsheep"))`
- const `DB_PATH` = `os.path.join(CONFIG_DIR, "agent-state.db")`
- const `SCHEMA` = `""" CREATE TABLE IF NOT EXISTS agent_state ( at REAL NOT NULL, pane_id TEXT NOT NULL, proj`
- const `_MODEL_FLAGS` = `("--model", "-m")`
- const `_THINKING_FLAGS` = `("--thinking", "--reasoning", "--effort")`
- class **StateStore**
  - `__init__(path)`
  - `_migrate()` — Add columns an older database is missing, without losing its rows.
  - `add(rows, at)`
  - `latest(pane_id, project, limit)`
- `project_of(cwd)` — The project label: the workspace directory name, or null.
- const `_SESSION_FLAGS` = `("--session", "--resume")`
- const `_AGENT_EXES` = `("claude", "pi", "codex", "opencode")`
- const `_SESSION_ROOTS` = `( os.path.expanduser("~/.pi/agent/sessions"), os.path.expanduser("~/.claude/projects"), os`
- `_proc_children()` — pid -> [child pids], from /proc.
- `_proc_snapshot()` — One /proc pass: (pid -> [children], pid -> process start time).
- `_proc_start_ticks(pid)` — Raw start-time ticks of a pid (field 22 of stat), or None.
- `_read_bound(pid, starts, reader)` — Read process metadata only when the identity holds BEFORE and AFTER.
- `_still_the_same_process(pid, starts)` — Is `pid` still the process the snapshot saw?
- `_descendants(pid, kids)`
- `_cmdline(pid)`
- `_open_session_file(pid)` — The session file this process has open, if any.
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

- const `SCHEMA_FIELDS` = `("message_id", "generation", "fingerprint", "first_seen", "last_attempt_at", "attempts", "`
- const `ACTIONS` = `("wait", "recover", "cooling", "reset", "terminal")`
- const `DEFAULT_THRESHOLD_S` = `120.0`
- const `DEFAULT_MAX_ATTEMPTS` = `2`
- const `DEFAULT_COOLDOWN_S` = `60.0`
- const `DEFAULT_OBSERVATION_INTERVAL_S` = `20.0`
- const `BLOCKED_STATUSES` = `("blocked", "dead", "unknown", "working")`
- const `_WS` = `re.compile(r"\s+")`
- `collapse(text)` — The composer's own wrapping is not content: compare on collapsed text.
- `fingerprint(text)` — Stable identity of the COMPLETE composer text.
- `_positive(raw, default, what)`
- class **Config** — Tunables, validated once.
  - `from_mapping(cls, raw)`
- class **Observation** — One look at one pane, at one instant.
  - `key()`
- class **Decision**
  - `__post_init__()`
  - `recovers()`
- class **Tracker** — Per-pane record of the tracked message, serialisable so a restart cannot duplicate an Enter: the attempt count survives, so a recovery is bounded across process lifetimes, not just within one.
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
- `verify(before, after)` — Independent consumption evidence after ONE Enter.

## dunk_superusers.py (288 lines)

> The superuser allowlist for destructive Dunking Sheep operations.  Only panes/agents on this list may close panes, tabs or workspaces, stop the

- const `CONFIG_DIR` = `os.environ.get("DUNKINGSHEEP_DIR") or os.path.expanduser( "~/.config/dunkingsheep" )`
- const `SUPERUSERS_FILE` = `os.path.join(CONFIG_DIR, "superusers.json")`
- const `DEFAULT_VP_NAME` = `os.environ.get("DUNKINGSHEEP_VP_NAME") or "VP of agentry"`
- const `DEFAULT_OWNER_NAME` = `os.environ.get("DUNKINGSHEEP_OWNER_NAME") or "owner"`
- class **SuperuserError** — A caller mistake (bad entry, unreadable allowlist).
- `_classify(entry)`
- `_lock_path(path)` — Sidecar lock file beside the allowlist (same directory/filesystem).
- `_locked(path, exclusive)` — Serialize allowlist writers against authorization readers (t423).
- `locked_allowlist(path)` — Hold a shared allowlist lock across check-through-mutation (t423).
- `_parse_allowlist(data)` — (panes, names) sets from decoded JSON; shared by load and locked reads.
- `match_allowlist(panes, names, pane_id, pane_info)` — Pure membership test shared by cached and freshly-read sets (t423).
- `read_panes_locked(path)` — (panes, names) re-read from disk under a shared lock (t423).
- class **Superusers** — Pane ids and names allowed to perform destructive operations.
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

## dunk_tasks.py (1934 lines)

> Global and per-pane tasks: the coordination backbone beyond context limits.  A task is a row in a SQLite database owned by the daemon, addressed through the

- const `CONFIG_DIR` = `os.environ.get("DUNKINGSHEEP_DIR") or os.path.expanduser( "~/.config/dunkingsheep" )`
- const `TASKS_DB` = `os.path.join(CONFIG_DIR, "tasks.db")`
- const `GLOBAL` = `"global"`
- const `STATUSES` = `("open", "in_progress", "blocked", "done", "cancelled")`
- const `PRIORITIES` = `("low", "medium", "high", "urgent")`
- const `PRIORITY_RANK` = `{"urgent": 0, "high": 1, "medium": 2, "low": 3}`
- const `DEFAULT_STATUS` = `"open"`
- const `DEFAULT_PRIORITY` = `"low"`
- const `DEFAULT_LEASE_S` = `float(os.environ.get("DUNKINGSHEEP_LEASE_S", "1800"))`
- const `_PANE_ID` = `re.compile(r"^(?:[A-Za-z0-9_.-]+:[A-Za-z0-9_.-]+|%\d+)$")`
- const `KINDS` = `("work", "review", "verify", "report")`
- const `DEFAULT_KIND` = `"work"`
- const `TERMINAL_STATUSES` = `("done", "cancelled")`
- const `VERDICTS` = `("accept", "needs-fix")`
- const `SCHEMA` = `""" CREATE TABLE IF NOT EXISTS tasks ( id TEXT PRIMARY KEY, scope TEXT NOT NULL, title TEX`
- class **TaskError** — A caller mistake (unknown id, bad status, bad scope).
- `_now()`
- `_as_list(value)`
- `_decode(raw)`
- `_encode(values)`
- `validate_scope(scope)`
- `validate_status(status)`
- `validate_kind(kind)`
- `validate_verdict(verdict)`
- `validate_priority(priority)`
- class **Tasks** — The task store.
  - `__init__(path)`
  - `_connect()` — This thread's connection, opened on first use and CLOSED when the thread ends.
  - `_prune_locked(current)` — Close connections whose owning thread has exited.
  - `connection_count()` — How many connections are open right now, WITHOUT pruning.
  - `__getattribute__(name)`
  - `_migrate()` — Add columns introduced after the first database was created.
  - `close()` — Close every thread's connection (the store is shared by the daemon).
  - `_next_id()`
  - const `NEXT_JOB_EVENT` = `"next_job_handoff"`
  - const `NON_BLOCKING_HANDOFF_STATES` = `frozenset({"cancelled"})`
  - `record_next_job_handoff(task_id, pane_id, from_task, message_id, attempt, reason, now, details)` — Record that `pane_id` was handed `task_id` after completing `from_task` (t372).
  - `_fire(task_id, event, **extra)` — Run `on_event` callbacks after a commit.
  - `_record(task_id, event, at, actor, scope, priority, status_from, status_to, duration_ms, meta)` — Append one immutable event.
  - `_statuses(ids)`
  - `_decorate(row, pane_id)`
  - `_full(row, pane_id)`
  - `_order_key(task)`
  - `_require(task_id)`
  - `add(scope, title, body, status, priority, assignees, parent_id, depends_on, created_by, multi_assignee, kind, author, repo, pr, head)`
  - `update(task_id, title, body, status, priority, parent_id, assignees, depends_on, block_reason, updated_by, multi_assignee)` — Change any mutable field.
  - `assign(task_id, assignees, mode)`
  - `complete(task_id, by, verdict)`
  - `set_verdict(task_id, verdict, by)` — Record a review verdict.
  - `set_accounting_pr(task_id, number)` — Record the accounting PR a review's accept verdict opened (FLAT_FLEET 6.4).
  - `notes(task_id, event)` — Every note event on a task, oldest first: [{at, actor, meta, text}].
  - `next_job_handoffs(pane_id)` — Latest durable state of each handoff, including terminal receipts.
  - `active_handoffs(pane_id, owned_task_ids)` — Handoffs that still count as "a prompt is on its way" for this pane.
  - `advance_next_job(pane_id, trigger, from_task, caller, unread, pending_delivery, dispatch_reason, operator_proof)` — Complete A, choose/claim B and journal its inbox id in ONE transaction.
  - `update_next_job(handoff, **changes, expected)` — Append progress under the writer lock, preserving the immutable chain.
  - `block(task_id, reason)`
  - `unblock(task_id)`
  - `cancel(task_id, reason)` — Cancel a task and SAY what it strands (t878).
  - `delete(task_id)`
  - `note(task_id, event, actor, meta)` — Append a history event without changing the task row (or needing it).
  - `recent_notes(task_id, limit)` — The last `limit` events that carry a reason, oldest first (t605).
  - `get(task_id, pane_id)`
  - `_visible_scopes(scope, self_pane_id)` — Which scopes a caller may see.
  - `list(scope, status, assignee, include_done, limit, self_pane_id, kind, needs_owner)`
  - `authors_of(repo, pr)` — Pane ids that WORKED on (repo, pr), from the work tasks (t723).
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
  - `heartbeat(task_id, pane_id, lease_seconds, caller)` — Extend a live lease.
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

- const `DEFAULT_TMUX_REGISTRY` = `os.path.join( os.environ.get("DUNKINGSHEEP_DIR") or os.path.expanduser("~/.config/dunkings`
- class **TerminalBackend** — The small contract every terminal backend implements.
  - `capabilities()`
  - `tab_labels()`
  - `list_panes_with_tabs()`
  - `list_panes_grouped()`
  - `agent_status(pane_id)`
  - `agent_session(pane_id)`
  - `self_pane_id()`
  - `resize(pane_id, cols, rows)` — Backends that cannot size a pane say so instead of pretending.
- class **HerdrBackend** — herdr, unchanged: the current default.
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
- class **TmuxBackend** — tmux backend: sessions are workspaces, windows are tabs, panes are panes.
  - const `PANE_SEP` = `"\x1f"`
  - const `PANE_FORMAT` = `PANE_SEP.join([ "#{pane_id}", "#{session_id}", "#{window_id}", "#{pane_current_path}", "#{`
  - const `TAB_FORMAT` = `PANE_SEP.join([ "#{window_id}", "#{session_id}", "#{window_name}", "#{window_index}", "#{w`
  - const `WORKSPACE_FORMAT` = `PANE_SEP.join([ "#{session_id}", "#{session_name}", "#{session_index}", ])`
  - `__init__(socket_name, registry_path, tmux_bin, guard_dir)`
  - `capabilities()`
  - `_run(args, timeout)`
  - `_tmux(*args, timeout)`
  - `version()`
  - `is_available()`
  - `server_error_hint()`
  - `_load_registry()`
  - `_save_registry(data)`
  - const `COLLECTIONS` = `{"pane": "panes", "tab": "tabs", "workspace": "workspaces"}`
  - `_remember(kind, item, **extra)`
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
  - const `RESIZE_MIN` = `1`
  - const `RESIZE_MAX` = `1000`
  - `resize(pane_id, cols, rows)` — Size the pane's WINDOW with tmux, then report what tmux actually did.
  - `_window_geometry(window_id)` — Measured cols/rows of every pane in one window, keyed by pane id.
  - `_client_count(session_id)` — How many clients are attached to the pane's session (unknown -> None).
  - `send_text(pane_id, text)`
  - `send_text_and_enter(pane_id, text)`
  - `send_keys(pane_id, keys)`
  - `read_pane(pane_id, lines, source, ansi)`
- const `DEFAULT_BACKEND_NAME` = `"tmux"`
- `backend_name()` — The backend that would be selected now: the env override, else the default.
- `get_backend(herdr)` — Return the configured terminal backend.

## dunk_tmux_guard.py (303 lines)

> tmux guard - installed by `dunkingsheep guard install-tmux`; do not edit.  The herdr guard's counterpart for the tmux backend. It shadows `tmux` *for the

- const `DUNKINGSHEEP` = `os.environ.get("DUNKINGSHEEP_CLI") or os.path.expanduser( "~/.local/bin/dunkingsheep")`
- const `GUARD_DIR` = `os.path.dirname(os.path.realpath(__file__))`
- const `NEWLINE` = `chr(10)`
- const `MARKER` = `b"dunkingsheep-tmux-guard"`
- const `READ_ONLY` = `{ "list-panes", "list-windows", "list-sessions", "list-clients", "list-keys", "list-comman`
- const `DESTRUCTIVE` = `{ "kill-pane": "pane", "kill-window": "tab", "kill-session": "workspace", }`
- const `TYPING` = `{ "send-keys", "send-text", "paste-buffer", "set-buffer", "load-buffer", "run-shell", "if-`
- const `GLOBAL_WITH_ARG` = `{"-L", "-S", "-f", "-T"}`
- const `GLOBAL_FLAGS` = `{"-2", "-C", "-D", "-l", "-N", "-q", "-u", "-v", "-V"}`
- class **Decision** — A pure close-policy decision: no tmux, no daemon, no side effects.
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
- `split_global(argv)` — Return (global_options, command, rest).
- `option_value(rest, name)`
- `current_target(kind, pane_id, globals_)` — kill-window/kill-session with no -t mean the current window/session.
- `own_pane()` — The pane this shell is in, or None.
- `route_close(kind, target, globals_)`
- `refuse(command, message)`
- `main(argv)`

## dunk_usage.py (210 lines)

> Configurable real-usage runner (owner #1110).  The usage config maps each provider to a command line, a timeout and a parse

- const `SCHEMA` = `1`
- const `_MISSING` = `object()`
- class **UsageError** — A config or invocation mistake; safe to show the operator.
- `_number(value, name)`
- `load_config(path)`
- `_path(value, dotted)`
- `_error(name, message, **extra)`
- `run_provider(name, spec, max_age)`
- `run(config, only, max_age)`
- `_windows(reading)`
- `render_text(results)`

## dunk_views.py (435 lines)

> Read-only compositions for the Command Center: roster, message state, project summary.  Three consumers (COS chat, the project page, the agent page) each need a shape that

- const `CONFIG_DIR` = `os.environ.get("DUNKINGSHEEP_DIR", os.path.expanduser("~/.config/dunkingsheep"))`
- const `APP_DB` = `os.environ.get("DUNKINGSHEEP_APP_DB", os.path.expanduser("~/workspace/vp-of-agentry/data/m`
- const `TRANSPORT_STATES` = `("sent", "read", "acked")`
- const `REPLY_STATES` = `("posted",)`
- const `MODEL_SOURCES` = `{ "claude": ("claude agents --json", "the session list carries no model field"), "codex": `
- `cell(value, as_of, reason)`
- `_iso(at)`
- `roster(flock, now)` — One row per lane: identity, state, current task, inbox, cost, cpu.
- `_cpu_for(flock, leases, now)` — CPU for a lane's caller pid, from /proc, or null with a reason.
- `_app_row(message_id)` — One row from the application's log, or None.
- `_app_state(row, now)` — sent / delivered / seen / replied for an APPLICATION message, with evidence.
- `app_messages(channel, limit)` — Recent application messages, newest first: what the owner chat renders.
- `message_state(flock, message_ids, now)` — sent / delivered / seen / replied for owner messages, with timestamps.
- `reply_stats(flock, mailboxes, now)` — Time from a message landing to the lane reading it: last and median.
- `_seen_latencies(inbox, mailbox, limit)`
- `_median(values)`
- const `PROJECT_REPOS` = `{ "dunkingsheep": "~/workspace/dunkingsheep", "vp-of-agentry": "~/workspace/vp-of-agentry"`
- `git_merges(repo, limit, timeout)` — Recent merges for one repository, with its own as_of.
- `project_summary(flock, project, path, now)` — Everything one project page needs, from the store and the DB.

## dunk_watchdog.py (1998 lines)

> Deterministic exception watchdog for the Dunking Sheep fleet.  No model is in this loop, and none may be added: every rule is a fixed

- const `SCHEMA_ID` = `"dunkingsheep-watchdog/2"`
- const `SEVERITIES` = `("info", "warning", "critical")`
- const `SEVERITY_RANK` = `{name: rank for rank, name in enumerate(SEVERITIES)}`
- const `CLASSES` = `("security", "outage", "money", "ops", "project")`
- const `OWNER_CLASSES` = `("security", "outage", "money")`
- const `SOURCE_RULE` = `"watchdog-rule"`
- const `SOURCE_SELF` = `"watchdog-self"`
- const `SELF_ERROR_RULE` = `"watchdog.self_error"`
- const `SYSTEM_SENDER` = `"watchdog"`
- class **ConfigError** — A named config problem: the dotted key and the reason.
  - `__init__(key, reason)`
- class **OwnerChannelUnavailable** — The owner transport is not configured; fail toward not paging.
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
- const `AMBIGUOUS_READER_MARKER` = `"ambiguous composer/transcript"`
- const `RULE_COMMON` = `{ "enabled": _bool, "severity": _enum(*SEVERITIES), "class": _enum(*CLASSES), "cooldown_mi`
- const `RULE_PARAMS` = `{ "fleet.assignment_starvation": { "quiet_minutes": POSITIVE, "min_empty_workers": POSITIV`
- const `TOP` = `{ "schema": _enum(SCHEMA_ID), "enabled": _bool, "dry_run": _bool, "cadence_minutes": POSIT`
- `_validate(spec, value, prefix)`
- const `HOLD_ATTEMPT_LIMIT_DEFAULT` = `10`
- const `RULE_DEFAULTS` = `{"severity": "warning", "class": "ops", "cooldown_minutes": 240, "must_hold_runs": 1, "ena`
- const `RULE_ADDITIONS` = `("delivery.ambiguous_queued", "fleet.assignment_starvation")`
- `migrate_config(config)` — Fill in rules a config predates (DISABLED).
- const `_RULE_EXAMPLE_DEFAULTS` = `{ ("fleet.assignment_starvation", "quiet_minutes"): 10, ("fleet.assignment_starvation", "m`
- `validate_config(config)` — Raise ConfigError naming the first bad key; return the config unchanged.
- `load_config(path)`
- `_expand(path)`
- `_ro(path)`
- `_load_json(path)`
- class **Inputs** — Lazily loaded, read-only views of the stores.
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
  - `name_to_pane()` — Mailbox name -> pane id, from the inbox's own routing history (the latest message wins, ties broken by id).
- `sample_host(path)`
- `finding(rule_id, rule, key, summary, threshold, measured, window, evidence, source, once)` — One finding.
- `_minutes(seconds)`
- const `HELD_OWNER_MARKER` = `"HELD FOR OWNER DECISION"`
- `_is_held(task)`
- `rule_task_blocked_stale(inputs, rule)`
- `rule_task_lease_stale(inputs, rule)`
- `rule_dunk_skip_loop(inputs, rule)` — A dunk that is still skipping.
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
- const `_AMBIGUOUS_SHAPE_RE` = `re.compile( r"^ambiguous composer/transcript: prompt row (\d+) is separated from footer ro`
- `_ambiguous_shape(reason)` — The integers in the documented sentence, or {} when the text is not that sentence.
- `rule_delivery_ambiguous_queued(inputs, rule)` — t409 OPTION 1: a message held on an AMBIGUOUS composer, said out loud, never acted on.
- `rule_host_load(inputs, rule)` — A band fitted to our own recorded work (2026-10-22), not to a guess.
- `rule_host_tmp(inputs, rule)`
- `_slot_holder_alive(slot)` — True if the slot records a live holder pid; None for a legacy slot.
- `rule_pane_retired(inputs, rule)` — Report how many panes are retired, in the info digest.
- `rule_resource_would_kill(inputs, rule)` — Report a lane that would trip a resource policy while it is doing its job.
- const `CARRIED_FAILURE_MAX_AGE_S` = `float(os.environ.get("DUNKINGSHEEP_CARRIED_FAILURE_MAX_S", "43200"))`
- `_recent_failure(failure, now)` — A delivery failure worth carrying: recent, and not already superseded by a success.
- `rule_host_heavy_slot_stale(inputs, rule)` — A poolrad-heavy slot is stale only when its holder is gone.
- class **RuleUnavailable** — A rule that has no input source: reported SKIPPED, never a pass or a page.
- `_text_records(inputs, since)` — (kind, id, text) for the content the secret scan covers: inbox bodies, task titles/bodies and dunk texts (VP i19821).
- `_secret_hits(text, patterns, allow)`
- `rule_security_secret_text(inputs, rule)`
- `rule_money_spend_projection(inputs, rule)`
- `rule_host_disk_full(inputs, rule)`
- `rule_health_failed(inputs, rule)`
- const `RULES` = `{ "fleet.assignment_starvation": rule_fleet_assignment_starvation, "task.blocked_stale": r`
- const `OWNER_ELIGIBLE_RULES` = `("host.disk_full", "security.secret_text", "money.spend_projection")`
- const `PANE_RULES` = `("inbox.unread_dead", "lane.working_without_task", "lane.idle_holding_task", "fleet.assign`
- const `REPLAYABLE_RULES` = `("dunk.skip_loop", "inbox.ambiguous_sender")`
- `_sort_key(item)`
- `evaluate(config, now, panes_provider, host_sampler, only)` — Run every enabled rule (or only those named).
- `self_error(problems)` — Exactly one self finding per run, carrying every raw problem.
- `empty_state()`
- `load_state(path)` — (state, problem).
- `save_state(path, state)`
- `dedupe_key(item)`
- `advance_streaks(config, state, findings, now)` — Consecutive-run counts per finding, as they would be after this run.
- `_cooling(config, state, item, now)`
- `plan(config, findings, state, streaks, now, owner_blocked)` — Decide, per finding, VP notify and owner page with the reason for any suppression.
- `recoveries(state, findings)` — Keys alerted before and absent now: VP recovery lines, plus one owner 'resolved' line only for pages the owner actually received.
- `_evidence(item)`
- `line(prefix, item, suppressed, streak, hold)`
- `render_lines(config, decisions, skipped, recovered, mode)`
- `digest(title, lines)`
- `inbox_sender(to, subject, body)` — VP delivery through the daemon's inbox API as the `watchdog` system sender.
- `send_starvation_group(config, state, item, now, send)` — Validated superuser destinations; remember each accepted send separately.
- class **DeskBriefChannel** — Confirm only the route selected by the bridge's shared routing reader.
  - `__init__(confirm_path)`
  - `desk_channel()`
  - `post(body)`
  - `confirmed_through()`
- `_confirm_pending(config, state, owner_channel, now, problems)` — Mark pending owner pages confirmed when the forwarder has passed them; report pages that stay unconfirmed past confirm_within_minutes.
- `run(config, now, dry_run_flag, send, owner_channel, panes_provider, host_sampler)` — One watchdog run.
- `write_heartbeat(path, now, cadence_minutes)` — The heartbeat records the effective cadence, so the dead-man can widen (never narrow) its limit when the two units disagree (VP i19870).
- `render_text(report)`
- `replay(config, start, end, step_minutes, panes_provider, host_sampler)` — Re-evaluate the history-backed rules at each step from `start` to `end` over the current stores, applying each rule's cooldown in memory, and count the VP notifications (and owner pages) each rule would have sent.

## dunk_watchdog_deadman.py (293 lines)

> Dead-man check for the exception watchdog (quis custodiet).  Deliberately independent of `dunk_watchdog`: it does not import it, read its

- const `RULE_ID` = `"watchdog.deadman"`
- const `SYSTEM_SENDER` = `"watchdog"`
- const `CLAMP_FACTOR` = `6`
- const `CAUSES` = `("daemon_down", "config_bad", "other")`
- `daemon_path_problem(daemon_path, expected_dir)` — A misconfigured daemon path raises the same FileNotFoundError as a dead daemon, so it is checked BEFORE anything is sent (VP i19851, w8:p8 i19854).
- `classify_daemon_error(error, daemon_path, expected_dir)` — daemon_down | config_bad | other.
- `heartbeat_age(path, now)` — Seconds since the heartbeat, or None if it does not exist.
- `recorded_cadence(path)` — (cadence_minutes or None, problem or None) from the heartbeat body.
- `effective_cadence(unit, recorded)` — max(unit, recorded), clamped to CLAMP_FACTOR x unit.
- `_load(path)`
- `_save(path, data)`
- `_read_error(path)`
- `daemon_sender(daemon_path)`
- `direct_write(inbox_path, target, subject, text)` — The fallback: one row straight into the inbox store, read back to verify.
- `check(heartbeat, cadence_minutes, multiplier, to, state_path, cooldown_minutes, now, send, dry_run, error_file, status_path, inbox_path, daemon_path, expected_dir)` — Return a report; send at most one alert.
- `main(args)`

## dunking_sheep_tui.py (783 lines)

> Dunking Sheep TUI - terminal interface for automated text sending.  This is a terminal-native sibling of Dunking Bird. Each "dunker" targets a terminal

- const `HERE` = `os.path.dirname(os.path.abspath(__file__))`
- const `REFRESH_MS` = `200`
- const `RECONNECT_S` = `2.0`
- `clip(value, width)`
- class **LocalFlock** — In-process fallback with the FlockClient call() interface, used when the daemon cannot be started (e.g.
  - `__init__()`
  - `call(cmd, **args)`
  - `close()`
- class **DunkingSheepTui** — Main terminal application.
  - `__init__(stdscr)`
  - `connect()`
  - `rpc(cmd, **args)` — Run a command against the daemon; errors become a status flash.
  - `refresh()`
  - `refresh_tasks()`
  - `run()`
  - `flash(text, seconds)`
  - `current()`
  - `current_id()`
  - `handle_key(key)`
  - `current_task()`
  - `handle_task_key(key)` — Keys for the tasks view.
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
  - const `TARGET_INDENT` = `4`
  - `target_modal(panes)` — Full-screen picker of terminal panes, grouped by workspace with the tab rows indented beneath each workspace name.
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

## herdr_client.py (490 lines)

> Thin wrapper around the `herdr` CLI, which speaks to the running herdr server over its unix socket and returns JSON. Dunking Sheep uses this the same way Dunking Bird used ydotool/kdotool: shell out to a trusted local binary.

- const `SEND_TEXT_CHUNK_CHARS` = `900`
- const `SEND_TEXT_CHUNK_DELAY_S` = `0.05`
- const `SEND_TEXT_SETTLE_DELAY_S` = `0.2`
- const `AGENT_KINDS` = `frozenset({ "pi", "claude", "codex", "gemini", "cursor", "devin", "agy", "cline", "omp", "`
- `_text_chunks(text, max_chars)` — Yield non-empty character chunks without splitting a CRLF pair.
- const `GUARD_DIR` = `os.path.join( os.environ.get("DUNKINGSHEEP_DIR") or os.path.expanduser("~/.config/dunkings`
- const `GUARD_MARKER` = `b"dunkingsheep-herdr-guard"`
- `_is_guard(path)` — True if `path` is the Dunking Sheep herdr wrapper rather than herdr.
- `_find_herdr()` — Locate the *real* herdr binary, preferring PATH then the usual install dir.
- class **HerdrClient** — Stateless helper.
  - `__init__(binary, default_timeout)`
  - `_run(args, timeout)` — Run `herdr <args>`; return (returncode, stdout, stderr).
  - `_run_json(args, timeout)` — Run a command whose stdout is a single JSON object; return the dict under `result`, or None on any failure.
  - `_run_result(args, timeout)` — Run a JSON-printing herdr command; return (True, result) or (False, message) with herdr's own error text when it refuses.
  - `version()` — The herdr CLI version string (e.g.
  - `is_available()` — True if the herdr binary exists and the server is reachable.
  - `server_error_hint()` — Human-readable reason the server looks unavailable.
  - `list_panes()` — Return a list of pane dicts (possibly empty).
  - `get_pane(pane_id)` — Return a single pane dict, or None if it no longer exists.
  - `list_tabs()` — Return a list of tab dicts (possibly empty).
  - `tab_labels()` — Map of tab_id -> human label for annotating panes.
  - `list_panes_with_tabs()` — Panes from `list_panes`, each annotated with a `tab_label` key.
  - `list_workspaces()` — Return a list of workspace dicts (possibly empty).
  - `list_panes_grouped()` — Panes annotated with tab + workspace labels/numbers and sorted by (workspace number, tab number) so callers can group them for display.
  - `_placement(cwd, label, focus, env)`
  - `create_workspace(label, cwd, focus, env)`
  - `create_tab(workspace_id, label, cwd, focus, env)`
  - `split_pane(pane_id, direction, cwd, ratio, focus, env)`
  - `start_agent(name, argv, workspace_id, tab_id, split, cwd, focus, env)` — Start agent `name` in a new shell pane.
  - `rename(kind, target_id, label)`
  - `close(kind, target_id)`
  - `focus(kind, target_id)`
  - `run_command(pane_id, command)` — Type a shell command + Enter into a pane (`herdr pane run`).
  - `send_text_and_enter(pane_id, text)` — Send literal text to a pane, then press Enter.
  - `send_keys(pane_id, keys)` — Press one or more named keys in a pane (`herdr pane send-keys`).
  - `send_text(pane_id, text)` — Send literal text to a pane without pressing Enter.
  - `agent_status(pane_id)` — Return the pane's agent_status (idle|working|blocked|unknown), or None if the pane cannot be found.
  - `read_pane(pane_id, lines, source, ansi)` — Return the last `lines` lines of a pane's output as text, or None if the pane cannot be read.
  - `agent_child_pids(pane_pid)` — Kernel process entries for the agent behind `pane_pid`.
  - `self_pane_id()` — The pane this process runs in, if herdr launched it.
  - `notify(title, body)` — Fire-and-forget herdr toast notification.

## text_editor.py (104 lines)

> Logical-line editing state for Dunking Sheep's curses text editor.  The standard-library ``curses.textpad.Textbox`` edits the curses window as one

- class **TextEditorBuffer** — Small, UI-independent multiline text buffer with a movable cursor.
  - `__init__(text)`
  - `text()`
  - `set_cursor(row, col)`
  - `insert(value)` — Insert text at the cursor, preserving logical newline boundaries.
  - `newline()`
  - `backspace()`
  - `delete()`
  - `move_left()`
  - `move_right()`
  - `move_up(amount)`
  - `move_down(amount)`
  - `move_home()`
  - `move_end()`

## Web routes (web/app.py as of PR #120, for T62)

- `post /api/login` (line 329)
- `post /api/logout` (line 351)
- `post /api/password` (line 366)
- `post /api/viewport` (line 428)
- `get /api/channels` (line 459)
- `get /api/pane-text` (line 481)
- `get /api/capability` (line 557)
- `post /api/pane-send` (line 574)
- `get /api/fleet` (line 683)
- `get /api/ops-metrics` (line 822)
- `get /api/ops-metrics/history` (line 836)
- `post /api/ops-metrics/refresh` (line 853)
- `get /api/ops-board` (line 867)
- `post /api/ops-board/refresh` (line 883)
- `get /api/game/status` (line 1250)
- `get /api/game/codewheel/{filename}` (line 1255)
- `post /api/game/start` (line 1268)
- `post /api/game/stop` (line 1276)
- `post /api/game/key` (line 1281)
- `post /api/game/type` (line 1290)
- `get /api/game/states` (line 1299)
- `post /api/game/savestate` (line 1304)
- `post /api/game/loadstate` (line 1312)
- `post /api/game/autocodeword` (line 1320)
- `get /api/game/video` (line 1332)
- `get /api/game/audio` (line 1340)
- `get /api/messages` (line 1348)
- `get /api/deliveries` (line 1368)
- `get /api/messages/{message_id}/context` (line 1388)
- `post /api/messages` (line 1416)
- `post /api/messages/{message_id}/send-now` (line 1480)
- `post /api/brief` (line 1543)
- `get /api/search` (line 1570)
- `post /api/uploads` (line 1584)
- `get /api/uploads/{attachment_id}` (line 1601)
- `get /api/panes/activity` (line 1621)
- `get /api/activity` (line 1656)
- `get /api/poolrad-dashboard/progress` (line 1737)
- `post /api/poolrad-dashboard/refresh` (line 1771)
- `get /api/poolrad-dashboard/dashboard.html` (line 1799)
- `get /api/poolrad-dashboard/state/reports/{filename}` (line 1810)
- `get /api/herdr-webui` (line 1847)
- `get /api/health` (line 1861)
- `get /api/version` (line 1866)
- `get /dash` (line 1919)
- `get /dash/` (line 1920)
- `get /dash/{path:path}` (line 1925)
