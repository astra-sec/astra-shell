# ADR 0008: Managed v3 with an explicit cold worker transition

Accepted 2026-10-27. Tasks SYNC-06/OPS-05 in astra-apple PROJECT_STATUS.md and Apple ADR0013.

The same bounded v3 ViewportTransfer used by rootless attachments now emits its existing viewport envelopes through the worker stream. The gateway validates route, negotiated format, fragment bounds and current QUIC payload size, and forwards each packet without reconstructing terminal state. It does not ACK, retain a replay buffer or expand quotas. Original worker ACK/latest-state/retry/reliable-keyframe logic remains authoritative. Public protocol and schema are unchanged; v1/v2 clients remain supported and Rust CLI continues to offer v2.

The internal identity handshake is ASTRAWK3/version3. Discovery rejects all previous internal workers before gateway listen, and old gateways cannot introduce a v3 worker. There is deliberately no internal N/N-1 adapter. Per-stream capability validation and per-connection UID/PID/epoch pin remain required. This uses ADR0006’s explicit maintenance rule: stop sessions and workers before deploying or rolling back. Never automatically remove a live incompatible worker/socket. WORK-03 rolling generations/draining and PTY migration are not implemented.

Maintenance preserves original multi-user state, certificate, per-user SSH authorization, resource limits and original UDP4433 listener. Do not overlay the separate rootless catalog. Existing KillMode=process intentionally leaves workers alive, so explicit maintenance must identify and retire those Astra process trees as well. Back up original binaries/units/state. Retain user files and backups; retire only the independent OPS04 unit and its port after successful original-port preparation. Router rules remain untouched.

Validation includes Rust negotiation/old internal rejection/fragment forwarding and bounds, real Apple→managed Quinn missing-fragment quiet-tail repair and resize/epoch/reconnect, same-worker same-terminal gateway restart, plus isolated Linux two-UID lifecycle and public old-client regression. Deployment results are recorded separately, not inferred from source completion.
