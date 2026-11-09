# Planner assignment scope (draft)

An owner explicitly grants a live planner pane a scope with `dunkingsheep lane planner grant --pane %9 --global` or `--project PROJECT`. Global membership alone does not grant planner authority. Revoke with `lane planner revoke --pane %9`; a restarted pane needs a new grant.

Associate a task with a project using `lane planner task-project --task t123 --project PROJECT`. A project planner can assign associated work and a global task currently listed as a direct dependency of an open, active or blocked project task; no separate blocker approval is required. A global planner can assign global work without a project association. Missing grants deny scoped planner assignments. Owner and service paths retain existing behavior.

The grant check and assignment use the same SQLite write transaction. The socket path checks recipient roles as worker or reviewer and, for project grants, their explicit project membership. Live role and process checks are point-in-time. This first slice does not fence older daemons, make role-file changes atomic with assignment, resolve task visibility restrictions, or prove reviewer independence. Trusted in-process calls without a socket identity retain existing behavior. Do not claim deployment-wide enforcement while an old daemon can write the database.

The owner associating a task accepts its existing dependencies. Subsequent socket-backed dependency edits to that associated task require an owner/service or a planner granted that same project, checked in the task transaction. Trusted in-process calls and older daemon versions can bypass that boundary, so deployment-wide provenance still depends on retiring those writers. Project visibility restrictions also remain.
