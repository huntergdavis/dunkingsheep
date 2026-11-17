# Projects, knowledge and shared resources

Projects is the working overview. A project's Work & decisions and Knowledge panels
load only when opened. Poolrad's reporting is available in its own project panel.
Projects from the explicitly configured knowledge-store map remain visible when no
agent is running; agent membership is still taken only from the daemon's records.
The fleet Tasks view remains available for work spanning projects.

The former Databases view is now Knowledge (`/dash/knowledge`). Its source remains
the existing context store: ground truths, authority, provenance, staleness and
supersession are preserved. Markdown presentation uses the existing sanitized
renderer. Viewing never promotes a fact into an instruction or writes the store.
Old `/dash/databases` bookmarks retain their query and redirect to Knowledge.

## Resource ownership

`GET /api/project-resources` is an owner-authenticated, no-store projection of the
shared provider usage cache and local host readings. It does not open operations
SQL, scan tasks, fetch GitHub, or construct an Operations board. Provider cadence,
backoff, last-good evidence, and stale/expired states are retained in
`usage_runtime.rs`. Usage reads begin on demand, not on a board refresh timer.

CPU utilization uses differences between `/proc/stat` counters (excluding guest
counters already included in user/nice). Its first sample is unknown. Memory uses
MemTotal minus MemAvailable; pressure is the kernel's 60-second `some` average.
Missing/unreadable measurements remain unknown. Saved temporary-disk health retains
its own observed time and stale marker. These are host/account readings shared by
projects, not claims of project-attributed resource consumption.

The Operations UI, aggregate builder/cache and refresh timer have been removed.
The historical operations database is retained because it still supplies Poolrad
reporting and verified accounting; no existing observations are deleted. Poolrad
history uses `/api/projects/poolrad/history`, and explicit collection uses
`POST /api/projects/poolrad/refresh`. The existing summary read remains at
`/api/ops-metrics` for compatibility. Merely opening Projects does not collect or
refresh Poolrad history. Old `/dash/operations` bookmarks open Projects.

## Task read and decision paths

The task execution engine, leases and review policy are unchanged. Paged task reads
accept an exact `project` filter against `planner_task_projects`, before pagination;
the project is bound into the cursor. The web reader verifies this filter was
acknowledged and refuses incompatible older daemon responses, rather than showing
cross-project results. Kind and needs-owner filters also run in the daemon.

Owner task details expose the recorded brief. Decision controls use the existing
versioned owner-question/answer protocol at `/api/tasks/question`, `/api/tasks/answer`
and `/api/tasks/not-now`. Authentication and origin checks remain in the route layer;
version/ownership authorization remains in the daemon. No viewer action claims work.

## Release order and rollback

Deploy the daemon's additive project paging support before or together with the desk
and UI. Stage and verify the candidate independently; do not replace the installed
build for testing. An independent reviewer must accept the exact head before merge.
After main is green, ask the VP to use the normal release procedure to restart the
live services and verify Projects, Knowledge, scoped tasks, owner decisions, and
shared resource readings in the signed-in desk. Keep the prior release for rollback.

No new runtime dependency, database migration, task-system replacement, or external
monitoring/wiki service is introduced. The Python bridge/oracle files remain
historical compatibility evidence; the production board aggregator is removed.
