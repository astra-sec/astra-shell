# Transport streams v1 and semantic DATAGRAM delivery

Status: implemented for `NET-01`, `NET-02`, and `SYNC-03` on 2026-09-04.

## Stream contract

After authentication, peers that negotiate `transport.stream_hello` v1 send `StreamHello` as the
first frame of every bidirectional application stream. It declares:

- `kind`: control, terminal, or file;
- `handle`: the Terminal ID for a terminal stream, otherwise empty;
- `epoch`: an optional 16-byte terminal epoch;
- `request_id`: the non-empty ID of the immediately following Request.

The gateway validates the descriptor against the Request before dispatch. Managed mode propagates
only validated metadata through additive `WorkerStreamHello` fields. N-1 peers that do not negotiate
the capability retain the registered request-inference compatibility path.

The Rust client exposes framed send/receive wrappers instead of Quinn stream types to terminal and
file code. Terminal, control, and file streams receive priorities 10, 0, and -10. Client and server
explicitly enable Quinn's same-priority fair queuing, so streams within a class are round-robin while
interactive classes precede bulk file traffic.

## DATAGRAM contract

`terminal.datagram_state` v1 requires `transport.stream_hello`, `terminal.semantic_diff`, and
`session.objects` (which in turn preserve the semantic-state/ACK dependency chain). A
`TerminalStateDatagram` contains an Attachment UUID and one complete cumulative diff.

The encoded payload limit is 1200 bytes. Larger diffs and all snapshots use the reliable stream.
After a successful DATAGRAM send, the worker retains the exact prepared update and retransmits it on
the reliable attachment stream after 100 ms unless its generation is acknowledged. Managed workers
send an internal `WorkerDatagram` envelope containing both the unreliable payload and an encoded
reliable `TerminalEvent`; gateway DATAGRAM failure therefore falls back immediately without
reconstructing protocol semantics.

Server DATAGRAM buffers are capped at 64 KiB. Apple additionally caps the connection receive queue
at 64 datagrams and each Attachment route at 8. Overflow is permissible because the reliable timed
fallback remains authoritative.

