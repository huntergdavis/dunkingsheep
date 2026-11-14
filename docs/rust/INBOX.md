# Inbox slices (t4684)

The owner Inbox first uses the existing owner conversation projection. That projection includes
`owner` and mailboxes attached to registered owner-role panes, filters routine lane mail, and
exposes up to 200 recent rows through `/api/owner-thread`. It is not a complete mailbox archive.

## Mailbox directory: S-B

`GET /api/mailboxes?limit=50&after=<URL-encoded mailbox>` returns a counts-only directory for
an authenticated owner **session**. API bearer identities are refused (403); unsigned requests
receive 401. It uses the existing desk same-origin checks and production path routing.
No daemon role gains additional access.

The route calls only `inbox_stats` and `roster`. It does not read, acknowledge, archive or notify
any mailbox. It does not resolve mailbox labels into target panes. No subject or body is
returned, even if the RPC adds fields later. The index inherits `inbox_stats`' exclusion of
cancelled rows and mailboxes with no non-cancelled messages.

The response contains `mailboxes`, `total`, `limit`, `next_after` and `roster_state`. Each row has
its exact mailbox key, destination label, unread/total/archived/undeliverable counts,
`oldest_unread_s`, `mapping`, and a `lanes` array. Lane metadata contains only pane ID, name,
harness, and explicit project membership. Counts have the daemon's semantics, not a new
estimate. Missing RPC fields remain null.

Rows are ordered by the exact mailbox key. `limit` defaults to 50 and must be 1..100. Follow
`next_after` using URL encoding until it is null. The cursor is exclusive, so unread count
changes do not reorder pages. This is a live directory, not a snapshot: a new mailbox inserted
before the current cursor appears on refresh. `after` must be nonempty, at most 1024 bytes,
and contain no control characters. Malformed paging parameters return 400 before any RPC.

A mailbox joins roster rows by name or pane ID using the daemon's existing mailbox
normalization (including legacy trailing pane suffixes). Every matching lane is returned;
colliding names are not arbitrarily assigned to one project. Membership is reported as supplied
by the roster, never inferred from a working directory. `mapping=unmapped` means no current
roster match; it does not prove the lane is dead. If roster data is unavailable, counts remain
usable with `roster_state=unavailable`, `mapping=unavailable`, and no lane assignments. Failure
of mailbox statistics returns 502 instead of an empty directory.

This slice supplies the directory endpoint. Mailbox message summaries, complete history paging,
reading already-consumed lane mail, the mailbox-browser UI, and Inbox compose are subsequent
slices. The endpoint alone does not claim those experiences are available.
