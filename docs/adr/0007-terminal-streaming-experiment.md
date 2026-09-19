# ADR 0007: Latest-state-wins terminal streaming

Status: experimental v2; implemented by the Rust server, worker bridge, supervised client library, and opt-in CLI

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

The Rust experiment separates the protocol into four delivery classes:

1. Input, resize, attachment control, and leases remain reliable and ordered.
2. A live viewport keyframe is reliable. It contains exactly one primary and
   one alternate viewport, never primary scrollback.
3. Live deltas use QUIC DATAGRAM when they fit the path payload budget. Each
   delta is cumulative from an explicitly named retained base generation. It
   never depends on the immediately preceding datagram. Unchanged palette,
   mode, style, hyperlink, title, and working-directory metadata is inherited
   from that exact base rather than repeated in every delta.
   V2 omits unchanged rows and encodes changed cells as a contiguous splice;
   screen dimensions change only through a keyframe.
4. History remains reliable and independently paged by stable row anchors.

The sender does not stop after one unacknowledged generation. The receiver
atomically commits the newest reconstructable generation, drops older or
duplicate generations, and reports an ACK after replica commit rather than
after drawing. An ACK may advance the sender's delta base to any retained sent
generation. Its connection-level datagram router gives each attachment a
single latest-value mailbox rather than a FIFO, so a slow renderer consumes
constant memory and never has to drain obsolete generations.

If the newest delta is larger than the datagram budget, the sender immediately
starts a reliable keyframe **only if the preceding keyframe has been ACKed**.
While it is in flight, all newer states replace a single pending target. A
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
- Reliable keyframes are ACK-gated, not merely timer-limited. Running attachments
  allow one in flight; terminal exit permits one additional final state before
  Exited. Reliable output drains separately from input/lease processing.
- A stalled renderer retains at most one undelivered viewport datagram per
  attachment.
- Reliable state/history transfers are atomically assembled and cannot be
  replaced by a different transfer before completion.

## Integration boundary

Rust peers advertise `terminal.datagram_state` v2 only together with semantic
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
newest observed generation. The sender emits its latest state as a keyframe,
subject to the same one-in-flight gate. Retired-epoch ACKs/repairs are ignored;
an unknown epoch is rejected. A one-second bounded re-key timer remains as fallback if
the request itself cannot complete before the connection fails.

Swift support remains deliberately out of scope for this Rust-first change. The
Apple client must not offer `terminal.datagram_state` until it implements the
same replica validation, routing, ACK, repair, and MTU rules.

Every datagram carries both terminal and attachment identity. This is required
to demultiplex simultaneous shells sharing one authenticated QUIC connection;
generation numbers are scoped to a terminal epoch and are not connection-wide.

Reliable reads retain framing progress across cancellation. Live export walks
only viewport rows and is cached until the engine advances/resizes; the 16ms
sampling clock advances even if no state can be sent. Network and renderer
progress are independent. `StreamingConnection` serializes shared connection
recovery, while each `LiveTerminal` resumes its own identity/lease with bounded
exponential backoff, drops offline input, and publishes only latest state.

`astra --streaming user@host [attach ID]` exercises this path. The default CLI
remains legacy for interoperability. V2 does not negotiate the withdrawn
experimental v1 encoding. See `../protocol/terminal-datagram-state-v2.md`.

## Verification

The Rust suite covers exhaustive bounded loss/reordering convergence, quiet-tail retry,
oversize coalescing, explicit repair, reliable keyframe/history integrity,
latest-value receiver coalescing, N-1 unknown fields, a real Quinn datagram
round trip, the managed worker/gateway bridge, and two live terminal
attachments (including history paging) demultiplexed over one authenticated
QUIC connection. The client integration also closes that connection, resumes a
persistent terminal and its input lease on a newly authenticated connection,
then verifies reliable re-key followed by live datagram delivery.

Review regression tests additionally interrupt reliable reads at every byte
boundary, deliver late epoch ACKs, ACK retry-only generations, suppress keyframe
ACKs, reorder mailbox arrivals, and test one-cell edits in dense 24x80 through
60x180 viewports against an actual 1162-byte Quinn payload budget. Their v2
payloads are 296–347 bytes (keyframes 18,835–102,398 bytes); these are encoded
payload measurements, not total network traffic or universal TUI benchmarks.

A real UDP relay test drops every 11th packet, delays alternating packets by
5/30ms to reorder them, interrupts the connection during a bidirectional outage,
and verifies two supervised terminals resume. UI events are not consumed for
longer than the 15-second input lease, proving background ACK/renewal progress.
It also checks that offline input is not replayed and the newest resize survives.

An integration test launches the compiled `astra --streaming` inside a real
local PTY: input/echo, a 31x103 → 42x132 resize verified by remote `stty size`,
and clean remote-process/CLI exit. On macOS the nonblocking reader opens the
actual TTY name rather than the `/dev/tty` alias rejected by kqueue.

Large updates still use reliable transfer and are bandwidth/RTT limited. This
branch does not claim replaceable independent keyframe streams or 60fps at all
screen sizes. History is paged by the library; the experimental CLI does not
yet provide a dedicated scrollback browser. Swift adoption remains separate.
