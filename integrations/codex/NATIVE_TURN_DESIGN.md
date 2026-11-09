# Native Codex turn producer (work in progress)

The next slice replaces the Python producer, not the hook settings or live
installation. The native hook must run inside the Codex process tree and retain
session and Codex turn IDs, the daemon-issued token, and the starter PID and
process start identity. It must serialize each session, refuse stale, replaced
or expired ends, consume a matching token once before the end RPC, preserve
handoff IDs, and always exit zero on a daemon refusal. State and tokens must
remain private. Existing Python hooks stay available for rollback.

Acceptance requires isolated tests for successful token propagation and stale
or replayed stops, plus process-ancestry behavior. Existing source currently
releases and reacquires its state lock before end; the native implementation
must send an end only after it actually consumed the same generation under the
lock. Live per-lane installation is a separate step after review.

The wrapper selects an executable `dunk-codex-turn` sibling when explicitly
installed; otherwise it retains the Python producer. Building the binary does
not install it in a lane. Install or remove the sibling only with a reviewed
per-lane plan that preserves the old wrapper and producer for rollback.
