# Retired webmail UI

Bulwark is retired in favor of the native Inbox. It is not built, served, or shipped
with the desk. Its pinned source/build scripts remain historical reference only.
The UI prebuild removes previously generated public/webmail assets from old builds.
Old Mail bookmarks redirect to Inbox. Message storage and JMAP APIs are retained;
this retirement does not delete mail or prevent a future lightweight client.

## Retained adapter

The Rust adapter projects the daemon's human conversations through a limited
JMAP interface. Read/star/archive state lives in `mail-presentation.sqlite3`
beside the desk message database. These operations never acknowledge, cancel or
redeliver daemon messages. The original Inbox remains available.

## Archived client source

The pinned Bulwark submodule and integration scripts are historical reference.
Normal `npm run build` builds only the native desk and removes generated client
assets. `/webmail` and every nested path redirect to `/dash/cos`, even if stale
files are still on disk. The old `panel=webmail` bookmark also opens native Inbox.

The archived upstream source is AGPL-3.0-only; retain its license and provenance.

## Current boundary

This is an experimental, limited JMAP adapter, not a general-purpose mail server.
It exposes human conversations only. Internal submission targets
`desk@dunkingsheep.invalid`, reuses the guarded owner send route, and keys retries
to a persisted draft. It rejects external recipients and attachments. The
existing owner send route does not support attachments.

Only supported query filters/sorts should succeed; unsupported operations must
return explicit protocol errors. Full protocol conformance, incremental sync,
push, general mailbox creation, external email, and server-side filtering are
outside the initial integration. The adapter is not the default Inbox transport.

The native Inbox at `/dash/cos` uses the existing guarded send/reply routes.

## Verification

```
cd rust
ds-cargo test -p ds-web jmap --lib
```

Subjects are preserved in internal delivery as a `Subject: ...` line preceding
the body. A submission reserves its draft before contacting the daemon. A crash
or uncertain response leaves it reserved; retries cannot silently redeliver it.
Check the existing Inbox to resolve that uncertainty. Sent presentation flags
remain separate from the submission record.
