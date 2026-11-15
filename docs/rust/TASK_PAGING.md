# Opt-in task list pages

Existing `task_list` calls retain their priority/age ordering, signed `limit`
semantics, default of 200, validation and response fields. Paging requires the
literal boolean `paged: true`; its `page_size` defaults to 50 and must be 1–100.
`limit` applies to the legacy mode only. No schema, task state, leases or events
are changed by listing.

Example socket arguments:

```json
{"paged":true,"page_group":"closed","page_size":50,"scope":"global"}
```

The normal task dictionaries are returned in `tasks`, alongside `scope`,
`pane_id`, `paging_version: 1`, `order: "id_asc"`, `total`, `page_size`,
`next_cursor` and `checked_at` (Unix seconds). `total` is the matching visible
count from the same read transaction as this page. A null cursor means the last
page, including empty results. IDs use binary string order (for example t1,
t10, t100, t2), independent of changing priority and age.

Send `next_cursor` unchanged as `cursor`, retaining the caller, scope, filters
and page size. The versioned cursor is query-bound continuation data, not an
authorization token. All scope authorization and visibility checks run again.
Malformed, oversized, unknown-version and mismatched cursors refuse. It is not
a frozen queue snapshot: status/assignment changes between requests can change
the matching count or membership; refreshing starts a new traversal. Deleting
the prior page boundary does not prevent continuing after its ID.

Paged mode supports existing status/kind/assignee/needs_owner filters plus a
`priority` filter. `page_group` optionally selects `active` (open/in_progress/blocked)
or `closed` (done/cancelled), and cannot combine with `status`. This separate
parameter avoids a collision with mutation status enums in the grouped MCP tool. Without an explicit status, `include_done` retains
its existing meaning. Empty/unknown results do not imply inaccessible queues are
visible. Caller scope rules match the legacy Flock path.

The store scans only matching ID/assignee metadata to count and select a page,
using the existing assignee decoder. Full dictionaries (including dependencies
and leases) are decorated only for the bounded page. Each page is one deferred
read transaction on the existing task actor; there is no cross-request snapshot
or server-side cursor state.

This daemon-only addition does not add CLI flags or a desk route. A later desk
Tasks view must detect `paging_version` and show an explicit first-page-only note
when using an older daemon. Do not infer paging support from an accepted request:
older handlers may ignore the new optional arguments.
