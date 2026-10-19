# Herdr upgrade playbook

Incident: herdr was upgraded to **0.9.5** on 2026-10-19 (~11:15 PDT). Every pane
came back as a **bare shell**; the agent roster had to be rebuilt by hand from
`daemon.log`, `dunks.json`, inbox history and the harness session files. The
guard was reinstalled and `dunkingsheep guard check` passed.

This playbook records that recovery and the commands that now automate it
(`dunkingsheep` **2.21.0**).

## Before an upgrade: keep the launch memory current

`launches.json` (`~/.config/dunkingsheep/launches.json`, 0600) records each
pane's harness, session, cwd and (for `start_agent`) the exact argv. The
daemon snapshots live `agent_session` metadata on its maintenance tick, so it
stays current automatically. To force a snapshot before an upgrade:

```sh
dunkingsheep recover --record      # snapshot live agent sessions
dunkingsheep doctor                # confirm herdr version + surfaces
```

## After an upgrade

```sh
herdr --version                    # expect herdr 0.9.5
dunkingsheep doctor                # read-only: surfaces + metadata warnings
dunkingsheep recover               # dry-run plan: pane -> action -> command
```

The dry run says, per running dunk target:

| action | meaning |
| --- | --- |
| `already-running` | the pane has an agent; **never** relaunch over it |
| `launch` | the pane is a bare shell and a command is known |
| `not-empty-shell` | the pane has content but no detected agent; inspect it |
| `no-session` | bare shell but no recorded/unambiguous session; start it manually |
| `pane-missing` | the target pane no longer exists; fix the dunk target |

Apply only the safe launches:

```sh
dunkingsheep recover --apply
```

`--apply` re-checks each pane at send time and types only into a pane that is
still a **bare shell** (`agent` absent, `agent_status` unknown, shell title).
It records the command back into `launches.json`. A missing session is
reported, never invented.

### Harness launch commands

| Harness | Detected from | Recovery command |
| --- | --- | --- |
| pi | `agent_session.kind=path` | `pi --session <session.jsonl>` |
| codex | `agent_session.kind=id` | `codex resume <id>` |
| claude | `agent_session.kind=id` | `claude --resume <id>` |
| opencode | — | no automated resume; start manually and `recover --record` |

## The manual steps that worked (2026-10-19)

1. **Find the panes.** `herdr pane list` (JSON envelope), or
   `herdr pane get <pane_id>`; a bare shell has no `agent` and a
   `terminal_title_stripped` like `hunter@cell: ~`.
2. **Recover the intended targets** from
   `~/.config/dunkingsheep/dunks.json`: `target_pane_id`, `target_agent`,
   `target_cwd`. Cross-check the last activity in
   `~/.config/dunkingsheep/daemon.log` and the inbox history.
3. **Find the session** for each agent from the harness files
   (`~/.pi/agent/sessions/...`, `~/.codex/sessions/...`,
   `~/.claude/projects/...`), or from the memory index
   (`dunkingsheep memory recall "<cwd or topic>"`). Use the real session; do
   not invent an id.
4. **Confirm the pane is empty** before typing:
   `herdr pane read <pane> --lines 20 --source visible`.
5. **Start the agent** in the pane with the harness command above, from its
   recorded `cwd` (e.g. `cd /home/hunter/workspace/<project> && pi --session <path>`).
6. **Reinstall/verify the guard**: `dunkingsheep guard install` (if the upgrade
   replaced the shadowed binary), then `dunkingsheep guard check`.
7. **Re-record**: `dunkingsheep recover --record` so the next upgrade is exact.
8. **Fix stale metadata**: `dunkingsheep doctor` flags a dunk whose
   `target_agent`/`target_cwd` no longer matches its pane. Re-target the dunk
   (`dunkingsheep update <id> -T <pane-or-label>`) so the stored metadata
   matches.

### Known warning on 2026-10-19

`d52` still carried `target_agent: "opencode"` on pane `wR:p3`, which is now a
**pi** pane. `dunkingsheep doctor` reports:

```
warning agent-mismatch: dunk says opencode, pane is pi
```

Re-target `d52` (`dunkingsheep update d52 -T wR:p3`) once the lane is settled;
until then the warning is expected and the dunk still delivers by pane id.

## Post-upgrade verification

```sh
dunkingsheep doctor          # pane_list / pane_get / read_visible / guard
dunkingsheep guard check     # guard intercepts pane typing
dunkingsheep health          # daemon + memory health
python3 -m unittest tests.test_dunk_core -q   # send / hold / flush / submit
```

Verified on herdr 0.9.5: `doctor` reports `herdr 0.9.5`, 16 panes
(10 agents, 9 sessions), `pane_list`/`pane_get`/`read_visible` true, guard
installed + shadowing; `recover` sees all five dunk targets as
`already-running`; the only warning is `d52`'s stale `target_agent`. The 137
`test_dunk_core` tests cover send, hold, flush and confirmed submit against the
0.9.5-shaped fakes, and a real 0.9.5 pane read is used as a fixture.
