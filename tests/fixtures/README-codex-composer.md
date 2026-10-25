# Captured codex composer screens (2026-10-24)

These are raw `tmux capture-pane -p -e` bytes from pane %17, kept because the
defect they show is about *layout*, and a hand-written screen would have missed it.

- `codex-composer-placeholder-%17.ansi` - transcript rows beginning with the same
  `›` sigil the composer uses; no hint row in this frame.
- `codex-pane17-transcript-plus-hint.ansi` - the decisive one: two transcript rows
  with `›`, and a BARE hint (`tab to queue message`) two rows above the status
  footer. `box_text()` returned the row-10 transcript text as the current draft,
  which is why m1843/m1844 were held as "someone is typing" for 5+ minutes while
  `delivery %17` said ready and the lane sat idle for eight hours.

DO NOT fix this by "the last sigil row is the composer": `tests/test_dunk_delivery.py`
requires a draft ABOVE a trailing empty prompt to survive (`test_a_draft_above_an_empty_prompt_is_still_found`,
`test_a_bordered_draft_is_found`), and that rule broke ten of those fixtures when
tried. The discriminator that does separate the two cases in these captures is:
a hint in the tail rows AND no prompt sigil BELOW the candidate line. That needs to
be built against the full fixture matrix, not patched in one sitting.
