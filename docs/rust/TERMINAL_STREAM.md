# Experimental desk terminal stream

The desk's `/api/terminals/{name}/stream` WebSocket is disabled by default.
For an isolated deployment, set `DUNKINGSHEEP_TERMINAL_STREAM=1` and set
`DUNKINGSHEEP_TERMINAL_STREAM_HOSTS` to a comma-separated list of exact Host
header values, including any port. The desk's configured external scheme must
match the browser's Origin. The handshake requires an owner session cookie,
one allowed Host and a matching explicit Origin.

The endpoint only selects uniquely named, registered non-owner agent panes
whose live tmux PID and process start match their role registration. It checks
the owner session and binding again while the stream runs. Those checks use
separate snapshots, not an atomic authorization transaction. Revocation can
occur between a check and an input or output operation; the one-second polling
interval is not a guaranteed revocation deadline. A timed-out tmux lookup
continues in a blocking worker and may keep the roles lock and its worker
permit until the subprocess returns. Four stuck workers can deny all new and
existing terminal streams; the permits do not gate the desk's other routes.

When enabled, the desk also serves `GET /api/terminals` to an authenticated
owner on an allowed host. It lists only uniquely named, live, non-owner agent
panes; state is null when no activity source is available. The listing is
informational and a stream revalidates its target independently.

The stream endpoint sends output produced after attachment and accepts binary
or text input frames. It does not send an initial screen snapshot. The existing
port 8792 terminal listing and resize API are unchanged; this endpoint alone
does not complete their access-control or proxy integration. An operator must
verify the intended gateway, Host, scheme, authentication, and live target
before enabling the feature. No service is enabled by this change.
