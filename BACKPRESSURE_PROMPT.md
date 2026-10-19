# Prompt for the dunky agent

Implement send backpressure for dunks, so a dunk never piles duplicate sends into a pane that has not consumed the previous one.

## The bug, observed for real

On 2026-09-12 a self-targeted dunk (`target="self"`, `only_when="idle"`, interval 240m) was pointed at an unattended Claude Code pane. Over about seventeen hours it fired four times. All four prompts queued in the terminal's input buffer and flushed into a single turn when the session next ran, so the agent saw four identical copies at once instead of four separate supervision passes.

The cause is in `dunk_core.py`. The `only_when="idle"` gate loops while `herdr` reports `agent_status` in `BUSY_STATUSES = ("working", "blocked")`. A pane sitting at a prompt with unread text in its input buffer reports `idle`, because no turn is in flight. The gate opens, the send lands on top of the previous unread one, and nothing ever consumes either. The gate has no notion of whether the last send was actually picked up. It has no backpressure.

## What to build

Add an opt-in per-dunk setting that makes a scheduled send a no-op when the previous send was never consumed. Suggested field name `skip_if_unconsumed`, a boolean defaulting to false. Keep it a separate field from `only_when` so the two compose: a dunk can be both idle-gated and backpressured.

Semantics to implement:

- When the dunk is about to send and `skip_if_unconsumed` is true, decide whether the previous send was consumed. If it was not, skip this send entirely. Do not increment `send_count`, do not touch `last_sent_at`, and do reschedule `next_send_at` to the next interval as normal. Set a distinct status such as `Skipped (unconsumed)` and record the skip.
- The very first send of a dunk always proceeds, since there is no previous send to consume.
- A skip is a normal outcome, not an error. Do not write it to `last_error`.

## How to detect consumption

Pick whichever of these you can make reliable, and say in your final report which you chose and why. I lean toward the first, with the second as a fallback signal.

1. **Busy transition since last send.** Record the target's `agent_status` right after each send. Between sends, the idle-gate poll loop already calls `herdr.agent_status` every `IDLE_POLL_S`. Latch a flag the first time the target is observed in a `BUSY_STATUSES` state after a send. If the flag was never latched when the next send comes due, the previous send was never picked up. Note the sampling gap honestly: a turn that starts and finishes entirely between two polls would be missed, so document that limitation rather than claiming perfect detection.

2. **Pane tail still shows the text.** Use `herdr_client.read_pane` and check whether the previously sent text is still sitting verbatim in the pane's last lines, which is what an unconsumed terminal input buffer looks like. This is a good cross-check but is sensitive to wrapping and to long texts that get chunked, so treat a match as evidence rather than proof.

Do not add a busy-wait or a new polling thread. Reuse the existing loop and poll cadence.

## Surfaces to update, all of them

The project's rule is that every surface stays in sync, so this is not done until all of these carry the new field:

- `dunk_core.py`: the `Dunker` field list and constructor, the send loop's gate section, and persistence round-tripping through `dunks.json`. Old state files without the field must load cleanly and default to false.
- `dunk_api.py`: the command registry schemas for `add_dunk` and `update_dunk`.
- `dunk_mcp.py`: the MCP tool schemas. These must stay flat, string/number/integer/boolean only, no unions, so strict validators keep accepting them.
- `dunkingsheep` CLI: a flag on `add` and `update`, plus display in `list` and `get`.
- `dunking_sheep_tui.py`: a key to toggle it, alongside the existing `o` for the idle gate, and show the state in the row.
- `dunk_help.py`, `README.md`, and `docs/`: explain the setting, and add it to the meta-dunking recipe, since a self-dunk on an unattended pane is exactly the case that needs it.

## Tests

Extend `tests/test_dunk_core.py` and the fake herdr in `tests/fakes.py` to cover, at minimum:

- First send always fires even with the setting on.
- Target never goes busy after a send, next send is skipped, `send_count` does not advance, and the dunk reschedules rather than stalling.
- Target does go busy after a send, next send fires normally.
- Setting off preserves today's behavior exactly, so existing dunks are unaffected.
- A skipped send does not populate `last_error`.
- Round-trip through the state file, including loading an old file that lacks the field.

Run the full suite before you commit. Do not weaken an existing test to make the new path pass.

## Finally

Commit with a clear message, and report back in one short paragraph: which detection method you chose, its known blind spots, and the exact flag and TUI key you settled on.
