# ADR 0007: Latest-state-wins terminal streaming

Status: accepted; implemented by the Rust server, worker bridge, and semantic client library

## Context

The current semantic-state path sends reliable snapshots or cumulative diffs,
but permits only one unacknowledged generation. Its effective update rate is
therefore bounded by network round-trip time plus client decode, commit, and
render time. A live terminal is different from input and control messages: an
old screen generation loses most of its value once a newer complete generation
is available.

Primary scrollback also amplifies live traffic even though history is already
addressable through reliable, paged requests.

## Decision

The Rust experiment separates the protocol into three delivery classes:

1. Input, resize, attachment control, and leases remain reliable and ordered.
2. A live viewport keyframe is reliable. It contains exactly one primary and
   one alternate viewport, never primary scrollback.
3. Live deltas use QUIC DATAGRAM when they fit the path payload budget. Each
   delta is cumulative from an explicitly named retained base generation. It
   never depends on the immediately preceding datagram. Unchanged palette,
   mode, style, hyperlink, title, and working-directory metadata is inherited
   from that exact base rather than repeated in every delta.
4. History remains reliable and independently paged by stable row anchors.

The sender does not stop after one unacknowledged generation. The receiver
atomically commits the newest reconstructable generation, drops older or
duplicate generations, and reports an ACK after replica commit rather than
after drawing. An ACK may advance the sender's delta base to any retained sent
generation. Its connection-level datagram router gives each attachment a
single latest-value mailbox rather than a FIFO, so a slow renderer consumes
constant memory and never has to drain obsolete generations.

If the newest delta is larger than the datagram budget, the sender coalesces it
as a pending state. A bounded timer promotes only the latest pending state to a
reliable keyframe, preventing a reliable-stream backlog per PTY generation. A
quiet-tail timer retransmits the latest unacknowledged state so that loss of the
last datagram still converges when PTY output stops. After a bounded interval
without ACK progress, or if the receiver no longer retains the named base, the
sender promotes the newest state to a reliable keyframe. Ongoing ACK progress
moves that deadline forward, avoiding periodic full keyframes on a healthy
stream.

## Validated invariants

- Dropping any proper subset of deltas cannot corrupt the replica.
- Reordering or duplicating deltas cannot move the replica backwards.
- Delivery of the newest delta, or its quiet-tail retry, converges exactly to
  the authoritative viewport state.
- Live keyframe size is independent of scrollback depth.
- A delta sent as a datagram is no larger than the connection's current maximum
  datagram payload.
- Reliable keyframes are rate-limited so repeated oversized states cannot grow
  a reliable-stream backlog without bound.
- A stalled renderer retains at most one undelivered viewport datagram per
  attachment.
- Reliable state/history transfers are atomically assembled and cannot be
  replaced by a different transfer before completion.

## Integration boundary

Rust peers advertise `terminal.datagram_state` v1 only together with semantic
State v2, state ACK v1, and formal session objects. Rootless attachments write
datagrams directly to their QUIC connection. Managed workers send one internal
`TerminalEvent.viewport_datagram` frame across the Unix stream; the authenticated
gateway validates its encoded size and lifts it onto the corresponding QUIC
connection. This internal event must never appear on a reliable client stream.

`WorkerStreamHello.maximum_datagram_size` carries the connection's negotiated
payload limit to the worker. Zero is required when the capability was not
selected. The worker cannot access or invent a client QUIC connection.

When a receiver cannot reconstruct a cumulative delta, it sends a reliable
`TerminalStateRepairRequest` naming the epoch, missing base generation, and
newest observed generation. The sender immediately emits only its latest state
as a reliable keyframe. A one-second bounded re-key timer remains as fallback if
the request itself cannot complete before the connection fails.

Swift support remains deliberately out of scope for this Rust-first change. The
Apple client must not offer `terminal.datagram_state` until it implements the
same replica validation, routing, ACK, repair, and MTU rules.

Every datagram carries both terminal and attachment identity. This is required
to demultiplex simultaneous shells sharing one authenticated QUIC connection;
generation numbers are scoped to a terminal epoch and are not connection-wide.

## Verification

The Rust suite covers exhaustive bounded loss/reordering convergence, quiet-tail retry,
oversize coalescing, explicit repair, reliable keyframe/history integrity,
latest-value receiver coalescing, N-1 unknown fields, a real Quinn datagram
round trip, the managed worker/gateway bridge, and two live terminal
attachments (including history paging) demultiplexed over one authenticated
QUIC connection. The client integration also closes that connection, resumes a
persistent terminal and its input lease on a newly authenticated connection,
then verifies reliable re-key followed by live datagram delivery.
