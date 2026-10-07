# ADR 0006: Versioned worker identity and safe discovery

Status: accepted, 2026-10-07. Tasks: WORK-01, WORK-02, FWD-02.
The user authorized completing managed forwarding and its prerequisites before
merging the feature. ADR 0005's per-UID worker and failure domain are unchanged.

Each worker generates an instance UUID once while holding its process lock. A
bounded internal v2 challenge/response precedes every gateway request. It carries
UID, PID and epoch; a QUIC connection pins the complete identity on its first
request. A replacement cannot receive requests from an old authenticated
connection: the gateway closes it with worker-epoch-invalidated and clients must
reconnect and enumerate the authoritative catalog. Other UIDs are independent.

Discovery must validate the runtime directory, socket ownership/type/mode and Unix
peer credentials before accepting the handshake. Authentication is the existing
Unix UID boundary; this is not protection against the same UID debugging its own
worker. Lock-file metadata changes use the opened descriptor. Gateway startup is
arbitrated across processes, not only across tasks, and stale worker sockets can
only be removed by the worker after acquiring its lifetime lock.

The former unversioned worker protocol is explicitly rejected, without killing,
unlinking or replacing the running worker. Updating those workers requires an
explicit maintenance operation once their sessions are empty. This is a fail-
closed migration, not a legacy adapter. WORK-03 rolling N/N-1 worker upgrades and
forced generation replacement remain separate work; no PTY migration is claimed.

Managed forwarding will run in the authenticated user's worker, using that UID's
network access (the same access already available to its Shell). No root-gateway
target sockets. This changes no OS firewall or user access policy. Per-user and
global capacity must include forwarding sockets/buffers before it is advertised.

Gateway startup revalidates and reserves capacity for surviving workers before
listening, including users who have not reauthenticated. Discovery/accounting is
serialized per UID; a failed probe cannot release another successful probe's
reservation. Dead kernel-verified PIDs are reaped on admission. Platforms without
a kernel peer PID retain reservations conservatively until gateway restart.
An unsafe or incompatible surviving worker makes startup fail explicitly; it is
never killed or silently replaced. Changing worker resource limits requires a
maintenance restart of workers (WORK-03 live policy changes are not implemented).
