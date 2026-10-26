# UI-18: independent full-state backpressure (2026-10-26)

This change is not deployed. Apple UI-18 dependencies are complete; WORK-03 remains
unfinished and this change does not implement worker hot replacement.

The endpoint reported for Icelake is port 4434. A pre-authentication Hello selected
semantic state v2 but no ACK/diff/DATAGRAM/compression/stream channels. This does not
inspect the negotiation of an already running attachment. Earlier port 4433
measurements must not be used as evidence of port 4434's capabilities.

An owned loopback rich TUI (1,025 loaded primary rows, alternate screen, 120 mouse
reports, 38 colored/hyperlinked UTF-8 rows rewritten per report) reproduced a
failure. The original error was `reliable attachment output budget exceeded`.
Many queued PTY notifications exported the same ~1.26 MiB full state and exhausted
the existing 20 MiB QueuedWriter. STOP_SENDING/QUIC cancellation followed. This is
separate from the client's mailbox-capacity failure in a different capability mode.

## Change

For semantic peers without ACK, Output/Resize/Lagged mark dirty. The existing
16 ms timer is not postponed by each notification. Its cancellable select future
waits for the reliable writer to drain before sampling. Under one engine lock,
epoch/generation are checked before full history export; duplicates are skipped.
Only revision metadata and dirty are retained, not another State or history.
The existing queue count, byte budget and protocol bounds are unchanged.

QueuedWriter already writes independently; previously `.await` on enqueue did
not wait on the network. Synchronous export/encoding could occupy the handler,
and the output queue could fill. The fix does not claim that network writing
itself had directly blocked all input handling.

Managed gateway forwarding propagates drain backpressure to the worker socket
only for independent semantic full states. Reverse input relay remains independent.
ACK/DATAGRAM forwarding keeps its existing policy. Drain is transport progress,
not evidence of client validation, display or acknowledgment.

Exit still sends final authoritative State before Exited. Lagged no longer replaces
the no-ACK receiver, preserving retained control/exit notifications. Other control
messages may precede a pending sampled state, as already possible in the ACK path;
no external effect is silently dropped or merged. Repeated control/history traffic
can still fail the existing bounded queue; no unlimited-traffic guarantee is made.

## Verification

- Library/bin tests: 178 passed, one manual benchmark ignored, CLI 6, daemon 2.
- Added duplicate revision, epoch/resize and cancel-safe drain tests passed.
- Managed relay test additionally transfers 96 × ~256 KiB reliable frames through
  1 KiB duplex buffers, holds output until reverse input succeeds, then drains
  slowly and verifies every complete frame in order. The true no-ACK branch passed.
- Clippy all targets with warnings denied passed. No temporary diagnostic prints
  remain in product source.
- Same loopback workload with new Apple Release: 120 sent/consumed/imported;
  input-end → import 697 ms, final consumption → import 109 ms. New Apple Release
  with the retained old server failed (120 sent, 83 consumed, 68 displayed).
- These are local rootless E2E plus managed relay tests, not a live Icelake or
  complete managed-host performance acceptance. No production worker was changed.

Full Apple report and evidence: sibling `astra-apple/docs/architecture/terminal-scroll-followup-20261026.md`
and its `.build/tui-scroll-followup-20261026/` directory. Existing workers hold their
own engine and PTYs; replacing only the gateway does not install this worker code.
A full production change requires a maintenance/session-ending decision until
WORK-03 provides a validated alternative. No such restart is authorized here.
