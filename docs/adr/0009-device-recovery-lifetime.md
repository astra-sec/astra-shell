# ADR 0009: Device recovery follows the live Terminal

Accepted 2026-10-28 for SESS-04, with Apple ADR0014. Dependencies SESS-01, NET-06 and PROTO-01 are complete.

The user requires automatic same-device recovery across disconnect, sleep and process restart while credentials and the Terminal remain intact. The previous seven-day eligibility deadline is therefore removed. One bounded device UUID/token record remains per live Terminal; the 15-second active lease, sequence checks and lease rotation are unchanged. A matching proof can recover after an arbitrary idle interval. Explicit RELEASE, another authorized takeover and Terminal destruction invalidate old authority. Device UUID or a shared SSH key alone is never proof.

When a durable owner record exists, a tokenless non-takeover grant is rejected even with no active lease. This applies to old peers too: they must use their existing explicit takeover action. A new Terminal without an owner still permits its initial grant. Old-client token resumption remains limited to its active legacy lease; no public wire field or capability version changes.

Every input/resize/renew/release command must match the nonempty lease captured by its attachment handler, before the existing current-lease/sequence validation. Correct Terminal/Attachment routing alone must not allow a viewer to borrow another attachment's lease. Viewer history/ACK/repair/detach remain supported.

Internal worker protocol remains ASTRAWK3. A gateway restart can reuse an old worker and therefore does not install these semantics. Production worker replacement is an explicit cold maintenance action, still outside WORK-03. No production restart or PTY termination is authorized by this implementation; prepare per-host ABI-compatible candidates and a precise impact/rollback plan first.

Current validation and rollout status are in sibling astra-apple `docs/architecture/client-identification-20261028.md` and `PROJECT_STATUS.md`; historical 10/13 tests are not fresh acceptance evidence.
