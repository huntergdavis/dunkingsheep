# Experimental desk terminal stream

The desk's `/api/terminals/{name}/stream` WebSocket is disabled by default.
Set `DUNKINGSHEEP_TERMINAL_STREAM=1` to enable it. The desk discovers its own
hostname, the hostname's `.local` name, configured DNS search-domain names,
and IP addresses currently assigned to local interfaces. It rechecks interface
addresses for each request, so a removed DHCP address does not remain trusted.
Authorities must use the actual listener port (`VP_PORT`, default8787); a missing
port is accepted only for HTTP80 or HTTPS443 when listening on that port.
`serve()` obtains the port from the bound socket, including an ephemeral port.

Optional `DUNKINGSHEEP_TERMINAL_LAN_DOMAINS` is a comma-separated list of DNS
suffixes for this machine's hostname. System resolver search/domain suffixes
are also recognized; no client-supplied name is resolved through DNS.
`DUNKINGSHEEP_TERMINAL_STREAM_HOSTS` is retired and ignored in production;
existing drop-ins may leave it set, but no address list needs maintenance.
If local discovery fails, only independently discovered names/addresses remain
usable; with neither, terminal access fails closed.

The desk's configured external scheme must match the browser's Origin. A
WebSocket handshake requires an owner session cookie, one local Host, and
exactly one explicit matching Origin. List/status requests require a matching
Origin whenever one is present. Forwarding headers never grant trust. A refusal
logs one bounded JSON line with Host, Origin, standard forwarding metadata,
actual remote socket address, listener port and the failed check; never cookies.
The login/session and target eligibility checks are preserved.

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

`POST /api/terminals/{name}/resize` accepts `cols` and `rows` from 1 to
1000. It requires the owner session, an allowed Host and matching Origin,
and exactly one active stream owned by this desk process. It refuses shared
or linked windows and windows viewed by another client. The tmux-side guard
accepts server versions 3.4 and 3.6 only; other versions fail closed. A timeout or other
uncertain result returns an outcome-unknown response: the operation may still
run. The unknown-outcome response includes a router-instance `epoch`. `GET` on the same
resize path reports `in_flight` and the current epoch. Only if epochs match,
wait for `in_flight` to become false and inspect the actual size before deciding
on a manual retry. A
worker retains a name-and-pane fence until it returns, preventing overlapping
resize attempts in that desk process. The fence is not durable across a desk
restart or router recreation; after an epoch change during an unknown outcome, independently reconcile the
tmux state before any retry. The role and process checks remain point-in-time; they are
not an atomic transaction with tmux or role changes.

If the resize worker panics, its fence remains busy until the router is
recreated because a dispatched tmux command may still execute. An epoch
change does not prove that command stopped; investigate before retrying.

The stream endpoint sends output produced after attachment and accepts binary
or text input frames. It does not send an initial screen snapshot. The existing
port 8792 terminal listing and resize API are unchanged; this endpoint alone
does not complete their access-control or proxy integration. An operator must
verify the intended gateway, Host, scheme, authentication, and live target
before enabling the feature. No service is enabled by this change.
