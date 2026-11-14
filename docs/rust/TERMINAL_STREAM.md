# Experimental desk terminal stream

The desk's `/api/terminals/{name}/stream` WebSocket is disabled by default.
Set `DUNKINGSHEEP_TERMINAL_STREAM=1` to enable it. The desk discovers its own
short hostname, that hostname's `.local` name, and current private-LAN interface
addresses. IPv4 is limited to RFC1918 (10/8,172.16/12,192.168/16); IPv6 is limited
to link-local and ULA. Loopback, public/CGNAT addresses and tunnel interfaces
(including Tailscale) are not automatically trusted. It rechecks interfaces for
each request, so removed DHCP addresses do not remain trusted.

Authorities must use the actual bound listener port (`VP_PORT`, default8787).
An omitted port is accepted only for HTTP80 or HTTPS443 on that listener.
`serve()` obtains the port from the socket, including an ephemeral port.
Resolver search-domain names are not automatically trusted. Existing explicitly
authorized DNS authorities in `DUNKINGSHEEP_TERMINAL_STREAM_HOSTS` remain accepted
at that same listener port; IP entries cannot bypass current LAN/interface
checks. No new address list or LAN-domain setting needs maintenance. Client
Host values are never resolved through DNS. If discovery is incomplete, only
independently discovered identities or previously authorized DNS names remain;
with neither, terminal access fails closed.

The desk's configured external scheme must match the browser's Origin. A
WebSocket handshake requires an owner session cookie, one local Host, and
exactly one explicit matching Origin. List/status requests require a matching
Origin whenever one is present. Forwarding headers never grant trust. A refusal
logs one bounded JSON line with Host, Origin, standard forwarding metadata,
actual remote socket address, listener port and the failed check; never cookies.
Refusal output is limited to one line per remote IP per60seconds (source-port
changes share the same budget). The next line includes its suppressed count.
Peer storage is capped at1024; excess/expired-peer counts are reported separately
as suppressed_untracked rather than creating an unbounded map or log flood.
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
