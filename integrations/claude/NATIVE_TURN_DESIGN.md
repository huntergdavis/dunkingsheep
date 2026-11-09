# Native Claude turn producer (work in progress)

The native producer preserves the existing Python producer's session and Claude
process PID/start binding, daemon token, serialization, TTL, handoff and silent
failure behavior. It accepts only `UserPromptSubmit` for start and exact `Stop`
with `stop_hook_active` for end; a `SubagentStop` must never close the parent.
Unlike Codex, Claude supplies no per-turn ID. This design relies on the existing
single-loop ordering assumption documented in `turn-generation.py`: a prior
Stop chain finishes before a later UserPromptSubmit. Duplicate stops consume one
state under the session lock. Tokens are created mode 0600 and never logged.

The sibling-aware wrapper leaves Python as fallback until a reviewed native
binary is explicitly deployed. The host's configured Claude wrapper currently
is a legacy shell hook, so this source change does not establish a live Python
runtime removal. Per-lane installation and real Claude verification are separate.
