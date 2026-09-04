# ADR 0007: Latest-state-wins terminal streaming experiment

Status: experimental; not advertised in protocol negotiation

## Context

The current semantic-state path sends reliable snapshots or cumulative diffs,
but permits only one unacknowledged generation. Its effective update rate is
therefore bounded by network round-trip time plus client decode, commit, and
render time. A live terminal is different from input and control messages: an
old screen generation loses most of its value once a newer complete generation
is available.

Primary scrollback also amplifies live traffic even though history is already
addressable through reliable, paged requests.

## Experimental design

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
generation.

If the newest delta is larger than the datagram budget, the sender coalesces it
as a pending state. A bounded timer promotes only the latest pending state to a
reliable keyframe, preventing a reliable-stream backlog per PTY generation. A
quiet-tail timer retransmits the latest unacknowledged state so that loss of the
last datagram still converges when PTY output stops. After a bounded retry
interval, or if the receiver no longer retains the named base, the sender
promotes the newest state to a reliable keyframe.

## Invariants to validate before integration

- Dropping any proper subset of deltas cannot corrupt the replica.
- Reordering or duplicating deltas cannot move the replica backwards.
- Delivery of the newest delta, or its quiet-tail retry, converges exactly to
  the authoritative viewport state.
- Live keyframe size is independent of scrollback depth.
- A delta sent as a datagram is no larger than the connection's current maximum
  datagram payload.
- Reliable keyframes are rate-limited so repeated oversized states cannot grow
  a reliable-stream backlog without bound.

## Integration boundary

This branch implements and tests the transport-independent Rust state machines.
It does not advertise a new capability or alter existing clients. Production
integration still needs a connection-scoped transport abstraction because a
rootless attachment owns a QUIC connection directly while a managed attachment
currently crosses a Unix worker stream. The gateway must frame worker viewport
updates and choose reliable stream versus QUIC DATAGRAM delivery; workers must
not attempt to use QUIC themselves.

The negotiated protocol also needs a small reliable repair request that maps a
receiver's `MissingBase` result to `rekey_latest`. The timer-based reliable
keyframe fallback guarantees eventual repair without it, but an explicit
request avoids waiting for that deadline.

Swift support is deliberately out of scope for this experiment.

Every datagram carries both terminal and attachment identity. This is required
to demultiplex simultaneous shells sharing one authenticated QUIC connection;
generation numbers are scoped to a terminal epoch and are not connection-wide.
