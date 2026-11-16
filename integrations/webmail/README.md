# Internal mail interface

This integration pins Bulwark Lite as a Git submodule. It runs inside the existing
Inbox tab and uses the existing owner web session. No email server, external
account credentials, IMAP or SMTP connection is configured.

The Rust adapter projects the daemon's human conversations through a limited
JMAP interface. Read/star/archive state lives in `mail-presentation.sqlite3`
beside the desk message database. These operations never acknowledge, cancel or
redeliver daemon messages. The original Inbox remains available.

## Build

From the repository root:

```
git submodule update --init integrations/webmail/bulwark
cd ui
npm ci
npm run build
```

The client build uses a disposable sibling workspace. Set `DS_MAIL_BUILD_DIR` to
an unused workspace directory to choose its location. Upstream Lite preparation
removes server-only source files, so the build never runs in the pinned submodule.
The normal UI prebuild runs this automatically and caches the generated assets by
upstream revision, integration script, and security patch. The overlay pins
Next.js 16.4.6 instead of the vulnerable upstream 16.4.3 lock entry.

Generated client files go into `ui/public/webmail` and then the normal UI bundle.
The upstream license is AGPL-3.0-only; retain its license and source provenance.

## Current boundary

This is an experimental, limited JMAP adapter, not a general-purpose mail server.
It exposes human conversations only. Internal submission targets
`desk@dunkingsheep.invalid`, reuses the guarded owner send route, and keys retries
to a persisted draft. It rejects external recipients and attachments. The
existing owner send route does not support attachments.

Only supported query filters/sorts should succeed; unsupported operations must
return explicit protocol errors. Full protocol conformance, incremental sync,
push, general mailbox creation, external email, and server-side filtering are
outside the initial integration. Browser acceptance and independent review are
required before making this the default Inbox.

Dependency rationale: reuse Bulwark's mail interface instead of implementing a
mail client. Fallback: the existing native Inbox, available at `/dash/cos`.

## Verification

```
cd rust
ds-cargo test -p ds-web jmap --lib
DS_MAIL_BUILD_DIR=/path/to/disposable/client ds-cargo test -p ds-web jmap_real_adapter_browser --lib -- --ignored
```

The browser test uses a synthetic daemon and an owner-auth fixture with the real
Rust adapter. It never connects to production. `DS_CHROMIUM` selects the Chromium
executable. Supply a disposable client build to retain its installed Playwright
and exported static files for this optional integration test.

Subjects are preserved in internal delivery as a `Subject: ...` line preceding
the body. A submission reserves its draft before contacting the daemon. A crash
or uncertain response leaves it reserved; retries cannot silently redeliver it.
Check the existing Inbox to resolve that uncertainty. Sent presentation flags
remain separate from the submission record.
