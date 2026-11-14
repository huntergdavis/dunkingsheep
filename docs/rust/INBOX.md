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

## Mailbox summary history: S-C

`inbox_list` retains its existing oldest-first output for CLI/MCP/TUI callers. Explicit
`newest_first=true` opts into a summary page with `next_before` (an exclusive message-ID cursor),
`order=newest_first`, and `audience="agent"` for agent-only mail (`null` for unmarked mail).
It defaults to unread-only, as before; use `unread_only=false` for history. Paged limits are
1..500. The query selects summary columns directly: bodies are not loaded or returned. Subjects
are included. This opt-in shape has `attachment_count`, not full attachment records.

`before` requires newest-first paging. Rows sort by creation time descending with message ID
as a deterministic descending tie-breaker, so timestamp ties neither skip nor repeat messages.
A cursor must still exist in the same mailbox; a removed/purged cursor refuses the page and
requires a refresh from the beginning. Cancelled messages remain excluded as in the existing list.

For browser mailbox keys, `literal_mailbox=true` additionally requires the daemon's owner, web
or system role. It validates and treats the key as data, never resolving a live pane/label or
reserved name. Other lanes cannot opt into this flag; their own-mailbox guard is unchanged.
Unmatched literal names return an empty page. This uses the existing mailbox normalization
for legacy spellings. No schema change or read/ack/recover/notification operation is involved.

`GET /api/mailbox-messages?mailbox=<encoded key>&limit=50&before=<encoded message ID>` requires
an owner session and same-origin checks, just like the directory. It always requests literal
newest-first history with `unread_only=false`; desk pages are 1..100 summaries. The response is
a typed field allowlist: no bodies or future daemon fields are forwarded, and malformed daemon
summaries return 502. Missing/expired cursors or RPC failure return 502 with refresh guidance.
An empty mailbox returns a successful empty page. API identities are refused before RPC.
The endpoint does not mark mail read or send notices. Reading bodies remains S-D, with its
separate security review; this slice alone does not add the mailbox-browser UI.

Rollout: deploy the daemon's opt-in paging support before the desk route. An older daemon's
legacy summary response lacks the required order/page fields and is refused as 502; the desk
will not silently browse a differently resolved mailbox. Existing desk routes and non-opting
CLI/MCP/TUI clients continue to work while the two components are rolled out separately.

## One-message body view and mailbox browser: S-D

The internal daemon verb `inbox_get(mailbox, message_id, include_agent=false)` is available only
to authenticated **owner and web roles**. It is not a new CLI or MCP command. The ID must be a
canonical positive `iNNN` counter value within i64 range, without leading zeros. The mailbox is
the exact persisted key returned on the summary row: no pane, substring, reserved-name or
legacy-alias resolution occurs. A missing ID or mismatched mailbox returns `status=not_found`
with no message. The handler never calls inbox_read/ack/recover or a notice operation.

Audience metadata is checked before a body is loaded. Agent-only mail returns
`status=agent_opt_in_required` unless `include_agent` is the actual boolean true; other truthy
values are refused. Successful results include the exact body, original state/read timestamp,
audience and attachment metadata. Each valid privileged lookup emits one JSON audit line to
the daemon journal: event, role, pane (null for the web service), mailbox, ID, opt-in and result.
No body, subject, sender or credential enters that logging interface. Failed/missing/opt-in-denied
lookups are also audited, without message text. Invalid inputs and denied roles do not read mail.

`GET /api/mailbox-message?mailbox=<exact key>&message_id=i42&include_agent=true` requires an owner
session and the existing origin checks. The flag defaults false. API identities are refused.
A typed response allowlist, exact key/ID checks, explicit audience validation, and `no-store`
protect the body response. Missing messages return404; missing agent opt-in returns403;
malformed/upstream failures return502. No message state is changed.

Inbox's **Browse all mailboxes** link opens `/dash/cos?panel=mailboxes`: a paged project/agent
mailbox directory, summary pages and a reading pane. The pinned Owner inbox makes complete
available owner-mailbox history reachable without the conversation's 200-row window. Unmapped
mailboxes stay visible, without claiming a lane is dead. Use Refresh for new mail; directory
pages and summary history are loaded on demand. The existing owner conversation and composer
remain reachable.

Agent-only summaries are labelled and collapsed by default, with disabled body-selection
controls until the browser's explicit opt-in is checked. The opt-in is not saved across visits.
Turning it off hides an already loaded or late-arriving agent body immediately. Messages use
the existing sanitized Markdown renderer; attachments are listed as text metadata (downloads
are not included in this slice). There is no reply action into an agent-only thread.

Release order remains daemon first, desk second. The body API has no fallback to a consuming
read or to the old daemon. This slice requires independent security review before release.

Deployment note: the queued daemon build3373428a carries S-C but not this new body verb. It
cannot enable S-D bodies. The browser treats502 as a soft unavailable state, mentions that a
daemon upgrade may be needed, and keeps Owner conversation reachable; no error fallback reads
or acknowledges mail. A daemon containing inbox_get must be live before S-D body browsing is
advertised as available.

Refused inbox_get calls are audited as metadata-only `result=refused`, including caller/role,
missing/unknown arguments and invalid inputs before body access. Unvalidated metadata is
bounded and replaced with `<invalid>` when it is not a safe key/ID; raw argument objects and
credentials are never serialized. Opening a folder/history page automatically previews its
first eligible message and produces a body-view audit line; this is not a read/ack receipt.
The owner role includes the authenticated owner seat, whose body access is intentional.
