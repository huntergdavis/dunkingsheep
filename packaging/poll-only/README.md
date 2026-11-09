# Poll-only release

`manage.sh` installs a hash-verified native poll binary and replaces only the poll
launcher; it snapshots that launcher for rollback. Supply existing canonical,
caller-owned bin and state directories, the SHA-256 of the reviewed binary, and
`--paired-rollback` pointing at the actual paired release prefix's `rollback`
directory. It refuses existing paired ownership. Roll back before any paired
release operation.

Both this tool and the accompanying paired release scripts use a persistent
per-bin lock and active poll marker under `${XDG_STATE_HOME:-$HOME/.local/state}`.
All cooperating operations must run as the same user with the same XDG_STATE_HOME.
Paired operations refuse while poll-only is active. Existing older copies of
paired scripts do not participate: operators must serialize those manually and
must not run them until poll-only has been rolled back. The caller must identify
all existing paired release ownership; the supplied path is not auto-discovery.
The private group exception requires a group named by the user's UID with no
supplementary listed members; deployments with other group arrangements are
refused. The daemon, CLI launcher and systemd drop-ins are not changed.

Interrupted install or rollback can be resumed by running rollback with the same
paths. If launcher drift is reported, inspect and reconcile manually rather than
overwriting it. Do not remove the state or lock marker while poll-only is active.
