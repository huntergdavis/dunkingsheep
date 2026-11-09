# OpenCode turn producer (in-pane)

This is the OpenCode half of t264.  OpenCode server plugins run inside the
OpenCode server process, so a plugin can call the daemon's turn RPC without a
pane label and the daemon authenticates the spawned CLI by ancestry under the
pane root.

## Events

| OpenCode event | action |
|---|---|
| `chat.message` for the pane's root session | `--json turn start` |
| `session.idle` for the session that started | `turn end --turn-id ...` |
| `session.deleted` for the root session | `turn end --reason session_deleted` |
| `session.error` | no end; the session may retry, the TTL is the backstop |
| child session events | ignored; a child cannot replace the pane's root |
| a second non-child root while a turn is active | refused as `ambiguous_session` |

The plugin keeps the daemon token in memory.  There is no end without a
successful start token.  A turn-end nudge's delivery receipt is the daemon
message-store record plus `metrics` kind=`message.sent` for that pane, not an
`inbox.db` `notify_state`/`read_at` (it is a pane send, not mailbox mail).  A plugin process restart, a reconnect to a different
server, or a hard crash loses the token: the daemon TTL (default 1800 s) expires
the turn.  That is the **stated** disconnect behaviour, not an inferred idle.
A shared/detached OpenCode server is not in the pane's ancestry, so the daemon
refuses its events; this plugin does not fall back to a cwd/session guess.

## Install

For a native installer, run `dunk-opencode-install ABSOLUTE_CONFIG_ROOT` with an
existing canonical caller-owned config root that is not writable by group or
others. The installer creates private `opencode/plugins` directories as needed,
preserves other plugins, and refuses an existing `dunkingsheep-turn.js` rather
than replacing it. Existing intermediate directories must also be caller-owned,
not group- or world-writable, and not symlinks. No config edit or OpenCode
restart is performed. The parent path must be trusted and no concurrent writer
should modify it during installation; cleanup after failure is best effort and
hard-kill or power-loss atomicity is not promised.

For an existing caller-owned config tree whose directories are group-writable,
`dunk-opencode-install --trust-group-writable ABSOLUTE_CONFIG_ROOT` explicitly
accepts that every member of those directory groups can modify the installation.
Use it only when those groups are trusted. The option does not change directory
permissions, permit world-writable or symlinked directories, or overwrite an
existing plugin. Trusting group writers also means the installer cannot prevent
them from changing files after installation.

The older Python installer remains available for existing overwrite workflows.

```sh
python3 integrations/opencode/install.py --check
python3 integrations/opencode/install.py
```

The plugin is copied to `<config>/opencode/plugins/dunkingsheep-turn.js`.
OpenCode loads every file in that directory, so no config edit is needed.
`--config-dir` selects an alternative XDG config root for an isolated test.

## Isolated live acceptance (2026-10-28)

An `opencode run` on `opencode/deepseek-v4.1-flash-free` in a test pane with an
isolated `XDG_CONFIG_HOME` reported `agent_status: null -> working -> idle`;
the producer log showed one accepted start and one accepted end
(`ended=true`), and the daemon log had exactly one `turn.end` for that pane.

That run also exposed why the end call is synchronous: `opencode run` exits
immediately after `session.idle`, and an awaited child process was abandoned
before the end RPC, which would have left the lane `working` until the TTL.
The producer now uses a short blocking `spawnSync` for the turn CLI, so the
token is consumed before the process can exit.  The TTL remains the backstop
for a hard kill or a reconnect that loses the in-memory token.

## Verify

```sh
node --test tests/test_opencode_turn_generation.mjs
```

The test points the plugin at a fake `dunkingsheep` executable and asserts:
start then token-bound end, duplicate start suppressed, a second end does not
reuse the token, no end after a failed start, a child session cannot start or
end the root, a different root while active is refused, and no `--pane`
argument is ever sent.
