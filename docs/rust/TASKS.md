# Rust port: the task list

Ordered; `depends_on` is the gate. Each row is one queue task
(`dunkingsheep task add --kind work`, body = the row's text plus the three
standing lines: "read docs/rust/RUST_PORT.md section 0 first; port tests from
the named files 1:1; record what you did NOT verify"). Sizes are for one lane
session. `MANIFEST.md` has every function name each task must produce.

Legend: **P1** phase 1 (cutover unit: daemon, CLI, MCP), **P2** timers and
read-only tooling, **P3** TUI and web.

## Foundation

| id | title | depends | creates | ports | tests (python -> rust) | acceptance |
|---|---|---|---|---|---|---|
| T00 | Workspace + manifest generator | - | `rust/Cargo.toml`, `rust/crates/ds-core/src/lib.rs`, `tools/gen_manifest.py` | - | - | `cargo build` empty workspace; `tools/gen_manifest.py` regenerates `docs/rust/MANIFEST.md` byte-identically from the tree |
| T01 | Differential harness | - | `tools/diff_harness.py`, `rust/fixtures/{traffic.jsonl,volatile.json,state/}` | - | - | `record` proxies the live daemon for 10 min and writes scrubbed traffic; `replay` against the Python daemon on a state copy reports 0 diffs; `golden` runs offline |
| T02 | error, config, clock, atomic_json, lock, retry | T00 | `error.rs config.rs clock.rs atomic_json.rs lock.rs retry.rs` | DunkError family (dunk_core 423-441), CONFIG_DIR/env switches (every `os.environ.get("DUNKINGSHEEP_` in the tree), `_locked`/`locked_allowlist` (dunk_superusers 60-130), the 15 atomic-write sites | test_dunk_superusers (store half) | mode 0600 on every written file; a lock held by one task blocks another; env table in config.rs matches `grep -o 'DUNKINGSHEEP_[A-Z_]*'` |
| T03 | proc.rs (dunk_callers) | T02 | `proc.rs` | dunk_callers.py entire: `_stat, process_start, process_alive, _cmdline, agent_name, caller_from_peer, caller_alive, caller_belongs_to_pane, caller_pane_auth_failure, pane_agent_pids` | test_dunk_callers | all reason strings identical (`caller_missing`, `pane_not_in_ancestry`, ...) |
| T04 | protocol.rs + registry.rs (table only) | T02 | `protocol.rs registry.rs` | dunk_server protocol doc; dunk_api `Command`, `COMMANDS` names/params/required/mcp, `SUPERUSER_ONLY`, `MCP_GROUPS` | test_command_smoke (schema half), test_dunk_mcp::grouped_home | `registry::COMMANDS.len()==117`; every MANIFEST command present with identical params; `input_schema()` JSON equals the Python's for every command (harness `golden --schemas`) |
| T05 | db.rs: writer actor + read pool | T02 | `db.rs` | the connect/WAL/busy pattern of dunk_sqlite.py and the three stores | test_dunk_tasks_threads (concurrency), new `db_tests.rs` | two writers serialize; a reader never blocks on a writer; busy maps to `DunkError::Busy` |

## Stores (P1)

| id | title | depends | creates | ports | tests | acceptance |
|---|---|---|---|---|---|---|
| T10 | roles.rs | T03 T02 | `roles.rs` | dunk_roles.py entire (ALLOW, WEB_DENIED, READ_ONLY, Roles, clamp_desk_*) | test_dunk_roles, test_dunk_superusers (gating half, against the roles file) | notify-based refresh; tables are `const`; `allowed()` truth table equals MANIFEST "roles allowed" column |
| T11 | tasks/store.rs | T05 | `tasks/store.rs` | dunk_tasks.py: SCHEMA, `_migrate`, validate_*, add/update/get/list/block/unblock/cancel/delete/note/assign/complete/set_verdict/_decorate/_order_key/events/_record/_fire | test_dunk_tasks (store classes), test_dunk_task_model | opens a copy of the live `tasks.db` and lists identical rows; migration test on the old-schema fixture |
| T12 | tasks/claim.rs | T11 | `tasks/claim.rs` | `claim_next, _claimable, _busy_task_ids, _refusal, _claim_locked, heartbeat, release, leases_for*, reap` with kinds/exclude_author | test_dunk_tasks (claim classes), test_dunk_task_pool, test_dunk_complete_ownership (store half) | BEGIN IMMEDIATE in the writer; refusal reasons identical |
| T13 | tasks/next_job.rs + advance | T12 | `tasks/next_job.rs` | dunk_next_job.py; `advance_next_job`, handoff records, `locked_allowlist` late check | test_dunk_turn_end_dispatch (store half), test_dunk_complete_ownership | late-authorization re-read inside the transaction |
| T14 | tasks/metrics.rs | T11 | `tasks/metrics.rs` | `metrics()` buckets, latency percentiles, leader_held (needs-owner) | test_dunk_tasks (metrics tests) | JSON identical for the golden fixture |
| T15 | inbox/store.rs | T05 | `inbox/store.rs` | dunk_inbox.py entire (messages, attachments by sha, counters, deliveries, digests, retention, `mailbox_where`) | test_dunk_inbox (store classes) | opens the live `inbox.db` copy |
| T16 | owner.rs, metrics.rs, panes.rs, activity.rs | T05 | those four | dunk_owner.py, dunk_metrics.py, dunk_panes.py, dunk_activity.py | test_dunk_owner_fds, test_dunk_metrics*, test_dunk_panes*, test_dunk_activity* | |
| T17 | stuck.rs | T02 | `stuck.rs` | dunk_stuck.py entire | test_dunk_stuck* | pure; tests 1:1 |

## Terminal (P1)

| id | title | depends | creates | ports | tests | acceptance |
|---|---|---|---|---|---|---|
| T20 | Terminal trait + FakeTerminal | T02 | `terminal/mod.rs`, `tests/fakes.rs` | dunk_terminal.py `TmuxClient` public surface; tests/fakes.py FakeHerdr | - | every method FakeHerdr has, with the same canned PANES/TABS/WORKSPACES |
| T21 | terminal/parse.rs | T20 | `terminal/parse.rs` | PANE_FORMAT/TAB_FORMAT/WORKSPACE_FORMAT parsing (`_parse_panes`, `_int`, title stripping, box_text) | test_dunk_terminal* (parse tests), test_dunk_tmux_guard (parse pieces) | fixtures: the exact `list-panes` output lines from the live host |
| T22 | terminal/tmux.rs (subprocess backend) | T21 | `terminal/tmux.rs` | every `_tmux(...)` call site (list-panes/windows/sessions/clients, capture-pane, send-keys -l/Enter/keys, respawn-pane, resize-window, select-pane -T, new-session/window, kill) | test_dunk_terminal* | behaviour identical; harness on `panes`, `read`, `send` |
| T23 | terminal/control.rs (control mode) | T22 | `terminal/control.rs` | NEW: `tmux -C` client, event parser, PaneTable, request matching, reconnect | new `control_tests.rs` with a recorded `%begin/%end/%output` transcript | `get_pane` is a map lookup; falls back to T22 on `DUNKINGSHEEP_TMUX_CONTROL=0` or attach failure; 1,000 `list_panes` calls < 50 ms |

## The Flock (P1)

| id | title | depends | creates | ports | tests | acceptance |
|---|---|---|---|---|---|---|
| T30 | flock/mod.rs: struct, construction, launches.json, lanes | T10 T11 T15 T16 T20 | `flock/mod.rs` | dunk_core `Flock.__init__`, `_record_launch`, `_snapshot_launches`, `_remember_lane`, resolve_* (target/mailbox/tab/workspace/_pick), `_pane_info`, caller state | test_dunk_core (resolve tests), test_dunk_identity* | |
| T31 | flock/roles_gate.rs | T30 | `flock/roles_gate.rs` | `caller_role, set_service_token, is_superuser, require_superuser, whoami, lane_register/unregister, token_issue/revoke, superuser_*` | test_dunk_roles (gate half), test_dunk_superusers | |
| T32 | flock/dunks.rs: Dunker + scheduler | T30 T22 | `flock/dunks.rs` | class Dunker, add/update/remove/start/stop/toggle/stop_all/fire/list/get, persistence (`dunks.json`), placeholders, only_idle, skip_if_unconsumed, max_sends | test_dunk_core (dunk tests), test_dunk_coalesce_freshness | one tokio task per running dunk; `dunks.json` format identical |
| T33 | flock/sends.rs: send_now, send_keys_now, typing guard, outbox | T30 T17 T22 | `flock/sends.rs` | send/delivery/outbox/queue/flush/clear_box/delivery_capability/delivery_target, `_desk_send_rights`, hold-while-typing, confirm-after-Enter | test_dunk_delivery, test_dunk_core (send tests), test_dunk_respawn_pane (send half), test_backpressure* | per-pane outbox task; harness on `send`, `send_keys`, `flush`, `delivery` |
| T34 | flock/commands_inbox.rs | T30 T15 | `flock/commands_inbox.rs` | inbox_send (PM redirect, system_sender gate, notice folding), inbox_read/list/count/ack/archive/fetch/delete/cancel/recover/status/stats, `_mailbox_or_self` own_only | test_dunk_inbox (flock classes), test_dunk_inbox_* | |
| T35 | flock/commands_tasks.rs | T30 T12 T13 T14 | `flock/commands_tasks.rs` | task_add (assign rules, kinds), list, read, update, assign, complete (verdict, authorize, github hook), block/unblock/cancel/delete/note, next (`_claim_filters`), heartbeat/release/follow/unfollow/followers/stats/metrics/reap, `_default_task_owner`, `_require_task_visible`, `_authorize_task_complete`, `_require_completion_binding` | test_dunk_tasks (flock classes), test_dunk_task_model, test_dunk_complete_ownership*, test_dunk_turn_end_dispatch | |
| T36 | flock/commands_turns.rs + loops | T30 T16 | `flock/commands_turns.rs`, `flock/loops.rs` | turn_start/turn_end/require_turn_caller, `_task_reaper_loop`, `_task_push_loop`, `_stuck_observer_loop`, next-job wake | test_dunk_turn_end_dispatch, test_dunk_activity* | loops are tokio tasks; push fires on turn end AND on the sweep (t615 rule) |
| T37 | flock/commands_layout.rs | T30 T22 | `flock/commands_layout.rs` | create_workspace/create_tab/split_pane/rename/focus/close(force)/start_agent(+`_verify_agent_started`, role registration)/respawn_pane/retire_pane/_sandbox_workspace_id/_title_pane | test_dunk_superusers (close tests), test_cli_respawn_dispatch, test_dunk_respawn_pane | start_agent never reports a false "did not start" for pi lanes (t599 fix) |
| T38 | flock/commands_owner_views.rs | T30 T16 | `flock/commands_owner_views.rs` | owner_push/take/poll/stats/receipts, roster/message_state/reply_stats/project_summary, state_capture/latest, status, health (daemon side), lanes/lane_set, capability | test_dunk_owner*, test_dunk_views*, test_dunk_server (status) | |
| T39 | api.rs: dispatch + handlers | T31-T38 | `api.rs` | dunk_api.py `dispatch()` (owner gate order, allowlist, desk clamps), every `h_*` as a typed args struct, `_authorise_send` retired path | test_command_smoke, test_dunk_mcp (grouped), test_dunk_dispatch_allowlist | harness: every P1 command 0 diffs on golden |
| T40 | server.rs | T39 T03 | `server.rs` | dunk_server.py entire: listener, per-connection loop, `_peer_pid`, `_bind_identity`, `handle_line`, supervised-start refusal, zero-panes refusal, die-with-parent, signals, `run_daemon` | test_dunk_server, test_dunk_identity_tty, test_dunk_complete_ownership_probe | `cargo run -- serve` passes the harness `replay` on recorded traffic for all P1 commands |

## Client, CLI, MCP (P1)

| id | title | depends | creates | ports | tests | acceptance |
|---|---|---|---|---|---|---|
| T41 | client.rs | T04 T03 | `client.rs` | dunk_client.py: resolve_identity (TMUX_PANE/HERDR_PANE_ID vs tty, refusals), `_current_pane_id`, FlockClient (connect retry, timeout, service token, system_sender rule), connect_or_start | test_dunk_identity, test_dunk_identity_tty, test_dunk_client* | `identity refused: ...` texts identical |
| T42 | ds-cli: generated parser | T04 T41 | `ds-cli/build.rs`, `src/main.rs`, `src/args.rs` | the argparse tree of `dunkingsheep` (156 subparsers, 362 arguments; aliases; `--json` at any depth; `--self-pane`, `--socket`, `--no-start-daemon`; exit codes 0/1/2/75) | test_cli, test_cli_json_position, test_cli_guard | `dunkingsheep --help` lists the same 76 commands; `insta` snapshots of every `--help` match the Python's |
| T43 | ds-cli: printers | T42 | `src/render.rs` | the 52 `print_*` functions | test_cli (output assertions) | `insta` snapshots against harness-captured Python output |
| T44 | ds-cli: local-only commands | T42 | `src/local.rs` | context reads (digest/get/search/check/stats/snapshot/index/ledger status), github protect/wrappers, install, guard install, git-credential, gh-token printer, doctor | test_dunk_context (CLI classes), test_cli (install) | |
| T45 | ds-mcp | T04 T41 | `ds-mcp/src/main.rs` | dunk_mcp.py: initialize, list_tools (role-filtered), call_tool (action resolve), protocol versions, structuredContent | test_dunk_mcp | `~/.claude.json` keeps `dunkingsheep mcp`; Claude Code lists the same tools |
| T46 | Phase-1 shadow run | T40 T42 T43 T44 T45 T01 | - | - | - | two days of hourly `replay` against the Rust daemon on a state copy: 0 diffs on P1 commands; p50 latency ratio recorded; `MANIFEST.md` "Error strings" all exercised |

## Phase 2: timers and tooling

| id | title | depends | creates | ports | tests | acceptance |
|---|---|---|---|---|---|---|
| T50 | context/{store,ledger,digest}.rs | T02 | those | dunk_context.py entire (front matter parse/render, Store, Ledger, digest_both, select_stores, check, index/search FTS, access log) | test_dunk_context, test_dunk_context_governance | reads the live `context/` tree identically; `context digest` byte-identical on the golden store |
| T51 | memory/{ingest,recall}.rs | T05 | those | dunk_memory.py (JSONL/sqlite ingest for claude/codex/opencode/pi, chunking, FTS5 recall/context/search/stats, redaction regexes) | test_dunk_memory* | recall results identical on the golden corpus |
| T52 | github.rs, review.rs, poolrad.rs | T35 | those | dunk_github.py, dunk_review.py, dunk_poolrad.py (tokens cache, kind_for_role, open_pulls paging, statuses/reviews/merge, Sync.run/on_verdict, attribute_author, backlog-sync, progress) | test_dunk_github, test_dunk_poolrad | fake GitHub tests 1:1; live: one `review sync --dry-run` identical to Python's |
| T53 | watchdog/{rules,deadman}.rs | T35 T34 | those | dunk_watchdog.py (every `rule_*`, digest, escalation classes, confirm path), dunk_watchdog_deadman.py | test_dunk_watchdog*, test_watchdog_* | `watchdog --dry-run` output identical on the golden state |
| T54 | dispatch.rs, state.rs, load.rs, policy.rs, health.rs, usage.rs, views.rs, pm.rs, help.rs | T35 | those | the named modules | their test files | each timer subcommand produces identical output on golden |
| T55 | allocator.rs, fallback.rs, recovery.rs, native_session.rs, http.rs | T35 | those | the named modules | their test files | |
| T56 | Phase-2 cutover of the timer units | T46 T50-T55 | packaging edits | - | - | every `dunkingsheep-*.service` ExecStart runs the Rust binary; one week of timers with no diff vs the Python outputs recorded in parallel |

## Phase 3: TUI and web

| id | title | depends | creates | ports | tests | acceptance |
|---|---|---|---|---|---|---|
| T60 | Performance gate | T46 | `rust/benches/` | - | - | `status` < 2 ms p50 at 30 concurrent clients; `inbox read` < 5 ms; `task next` < 10 ms; a 1,000-pane `list_panes` < 50 ms from the control-mode table; results in docs/rust/BENCH.md |
| T61 | ds-tui (ratatui) | T41 | `ds-tui/` | dunking_sheep_tui.py, text_editor.py | manual + snapshot | feature parity list in the task body |
| T62 | ds-web (axum) | T41 T50 | `ds-web/` | web/app.py routes 1:1 (see MANIFEST "web routes"), gameplay/character editor/codeword, session store, static ui/dist | pytest -> rust tests per route; browser capture pair (DESK_DESIGN 7) | same JSON per route via the harness `--http` mode |
| T63 | Delete the Python daemon tree | T56 T61 T62 | - | - | - | one release after T62; `packaging/`, `docs/`, `ui/` remain |
