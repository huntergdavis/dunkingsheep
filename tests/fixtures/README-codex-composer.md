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

## Conservative footer classification (t341)

Two blank rows between a prompt and footer cannot prove that the composer is
empty. `box_text` now returns empty text with an `ambiguous composer/transcript:`
detail for that geometry, unless an explicit placeholder has no other visible
content before the footer. Empty text alone is not an emptiness assertion.
Transcript fixtures remain excluded from draft text; ambiguous historical echo
frames also remain unknown rather than being silently promoted to clear.

Guarded sends, direct box checks and flushes refuse keys for this ambiguity;
delivery reports unknown and not deliverable. This can hold an owned notice or
a harmless echo until an unambiguous frame is available. It is a deliberate
refusal, not automatic recovery, and does not broaden inbox-prefix ownership.
Existing modal checks, prompt-region limits and contiguous/bordered draft rules
remain in force. Placeholder text followed by a visible continuation, including
a footer-shaped human line, is not proof of an empty composer.

`test_codex_ambiguous_composer.py` supplies synthetic ellipsised/wrapped notice,
human draft, mixed notice/draft, placeholder and transcript controls. It does
not contain an original capture of the t339 incident: no original ANSI frame
survives, and the operator's partial reconstruction cannot prove original
geometry, safe key authorization or live recovery. All existing capture files
in this directory are preserved unchanged.
