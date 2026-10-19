# MCP tool map (t8 grouping audit)

The declared MCP surface is one namespaced tool per domain, each taking an `action`. Nothing was removed: every flat name still resolves, and every command stays on the CLI/socket API.

- declared tools: **9** grouped (was **70** flat)
- grouped schema bytes: **15645** (was **42792**, -63.4%)
- hot set (`inbox`, `task`, `memory`): **5583** bytes (target <10 KB)

## Grouped tools

| Tool | Actions |
| --- | --- |
| `inbox` | `send`, `count`, `list`, `read`, `status`, `ack`, `cancel`, `delete`, `stats`, `fetch` |
| `task` | `add`, `list`, `read`, `update`, `assign`, `complete`, `block`, `unblock`, `cancel`, `delete`, `next`, `heartbeat`, `release`, `follow`, `unfollow`, `followers`, `stats`, `metrics`, `reap` |
| `dunk` | `list`, `get`, `add`, `update`, `remove`, `start`, `stop`, `stop_all`, `fire` |
| `pane` | `list`, `read`, `send`, `send_keys`, `flush`, `messages`, `get_message`, `cancel_message` |
| `workspace` | `list`, `create`, `create_tab`, `split`, `start_agent`, `rename`, `focus`, `close` |
| `superuser` | `status`, `add`, `remove`, `recover` |
| `memory` | `recall`, `context`, `search`, `stats`, `status`, `ingest`, `backfill` |
| `metrics` | `summary`, `status`, `health` |
| `system` | `help`, `herdr`, `herdr_help` |

## OLD flat tool -> NEW grouped tool

| Old tool | New tool | action |
| --- | --- | --- |
| `help` | `system` | `help` |
| `status` | `metrics` | `status` |
| `list_dunks` | `dunk` | `list` |
| `get_dunk` | `dunk` | `get` |
| `add_dunk` | `dunk` | `add` |
| `update_dunk` | `dunk` | `update` |
| `remove_dunk` | `dunk` | `remove` |
| `start_dunk` | `dunk` | `start` |
| `stop_dunk` | `dunk` | `stop` |
| `stop_all` | `dunk` | `stop_all` |
| `fire_dunk` | `dunk` | `fire` |
| `send_text` | `pane` | `send` |
| `send_keys` | `pane` | `send_keys` |
| `flush_box` | `pane` | `flush` |
| `list_messages` | `pane` | `messages` |
| `get_message` | `pane` | `get_message` |
| `cancel_message` | `pane` | `cancel_message` |
| `inbox_send` | `inbox` | `send` |
| `inbox_count` | `inbox` | `count` |
| `inbox_list` | `inbox` | `list` |
| `inbox_read` | `inbox` | `read` |
| `inbox_status` | `inbox` | `status` |
| `inbox_ack` | `inbox` | `ack` |
| `inbox_cancel` | `inbox` | `cancel` |
| `inbox_delete` | `inbox` | `delete` |
| `inbox_stats` | `inbox` | `stats` |
| `inbox_fetch` | `inbox` | `fetch` |
| `task_add` | `task` | `add` |
| `task_list` | `task` | `list` |
| `task_read` | `task` | `read` |
| `task_update` | `task` | `update` |
| `task_assign` | `task` | `assign` |
| `task_complete` | `task` | `complete` |
| `task_block` | `task` | `block` |
| `task_unblock` | `task` | `unblock` |
| `task_cancel` | `task` | `cancel` |
| `task_delete` | `task` | `delete` |
| `task_next` | `task` | `next` |
| `task_heartbeat` | `task` | `heartbeat` |
| `task_release` | `task` | `release` |
| `task_follow` | `task` | `follow` |
| `task_unfollow` | `task` | `unfollow` |
| `task_followers` | `task` | `followers` |
| `task_stats` | `task` | `stats` |
| `task_metrics` | `task` | `metrics` |
| `memory_recall` | `memory` | `recall` |
| `memory_context` | `memory` | `context` |
| `memory_search` | `memory` | `search` |
| `memory_stats` | `memory` | `stats` |
| `memory_status` | `memory` | `status` |
| `memory_backfill` | `memory` | `backfill` |
| `memory_ingest` | `memory` | `ingest` |
| `health` | `metrics` | `health` |
| `metrics` | `metrics` | `summary` |
| `list_panes` | `pane` | `list` |
| `read_pane` | `pane` | `read` |
| `list_workspaces` | `workspace` | `list` |
| `create_workspace` | `workspace` | `create` |
| `create_tab` | `workspace` | `create_tab` |
| `split_pane` | `workspace` | `split` |
| `start_agent` | `workspace` | `start_agent` |
| `rename` | `workspace` | `rename` |
| `focus` | `workspace` | `focus` |
| `close` | `workspace` | `close` |
| `herdr` | `system` | `herdr` |
| `herdr_help` | `system` | `herdr_help` |
| `superuser_status` | `superuser` | `status` |
| `superuser_add` | `superuser` | `add` |
| `superuser_remove` | `superuser` | `remove` |
| `superuser_recover` | `superuser` | `recover` |

## CLI/socket-only commands (not MCP tools)

| Command | Why |
| --- | --- |
| `shutdown` | daemon stop; CLI/socket only |
