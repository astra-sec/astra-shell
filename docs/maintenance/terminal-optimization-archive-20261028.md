# Terminal optimization archive — 2026-10-28

This archives the complete server change chain for UI-18, HIST-05, SYNC-05/06 and OPS-04/05. The current implementation is integrated through the original `main` target, preserving all ancestor commits. The authoritative cross-repository task status remains in astra-apple PROJECT_STATUS.md; UI-18 remains partial despite the user's report of markedly improved latency.

## Source history

Remote: https://github.com/astra-sec/astra-shell. Fetched original main: `eacd615f82df9c6ad29bfe3a0007ccdad7f3c740`. Validated and deployed source: `f02ebfe0c9face7878c6c1c57ed12e6445466de5`. Seven commits precede this documentation-only archive:

| Commit | Change |
|---|---|
| `e5e1eff` | Pace independent terminal snapshots with reliable backpressure |
| `33eb4b0` | feat: allow explicit reliable terminal delivery negotiation |
| `6ce700e` | feat: expose UDP offload control for incompatible network paths |
| `eb11d4c` | docs: record final private trial artifact and cleanup |
| `647fe20` | docs: close temporary trials and record restored Review deployment |
| `a5935c2` | Add bounded v3 cumulative viewport fragments for rootless servers |
| `f02ebfe` | Support managed v3 viewport delivery with cold worker version transition |

The historical `codex/ui18-legacy-worker` branch at `27909ab08bbfab8199f4f5446ada7b03b89cc5c5` is retained separately for the original-production-protocol maintenance experiment. Its four commits are also included in the remote archive, without merging old worker protocol/runtime code into current main:

| Commit | Historical change |
|---|---|
| `cc05c54` | Record production palette on verified legacy worker baseline |
| `e1ed8bd` | Backport full-state scheduling without changing legacy worker protocol |
| `67906b8` | Drain queued history before the final legacy worker state |
| `27909ab` | docs: record xy-only UI-18 maintenance deployment and limits |

That branch was a temporary current-worker repair, not persistent worker version routing. Its previously recorded full test suite had inherited timing failures and must not be described as fully green. Later OPS-04/05 deployments superseded it. Uncommitted `src/lib.rs`, `src/server.rs` and `src/history_projection.rs` in its separate worktree are unfinished working material, not accepted deployment source; they remain untouched and are excluded from this delivery. The committed historical branch is archived without those files' uncommitted changes.

## Deployment binding

Both original managed services run `f02ebfe0c9face7878c6c1c57ed12e6445466de5` at UDP4433. Actual gateway and xy worker executable hashes matched:

| Host | Original saved endpoint | SHA256 |
|---|---|---|
| Rome | `fit.zsq.im:4433` | `6c68febff9afcaab116041a296e4ec442b73f9ee321e2e36a76cea19d08579cd` |
| Icelake | `fit.zsq.im:4434` | `1a7ee0d407ea4a0c696e703141d076437ec21d9bd2f0e098336922eff38e7e89` |

Original router forwarding was not changed or staged. Original shared certificates, per-UID authorization, state and unit bytes were retained. Independent4435 units/autostart/listeners are removed; inert private data and backups remain. Mac Release1.1(5), product source `87b9b331f59b01c47a6c3910ba6cfcfed54968d7`, executable SHA256 `ef4282e4f29f9e5c292676ab5b7e151f115c8cd85cfd1d8773579f17d9538cd5`, passed original identity authentication, v3/ACK, colored Chinese input/output and same-PTY reconnect on both endpoints. Its diagnostic process exited and normal signed launch was restored.

The first Icelake attempt used a Rome artifact requiring GLIBC2.39 against Icelake2.35 and failed. Original services were restored; the exact source was rebuilt on Icelake, tested, and pre-executed before the successful second maintenance stop. Do not reuse cross-host binaries without target-runtime validation. Full failure, backup permissions and rollback boundaries are in the [OPS-05 report](https://github.com/astra-sec/astra-apple/blob/main/docs/architecture/original-ports-managed-v3-20261027.md).

## Validation and limits

Recorded validation: Rust191 library+6 CLI+4 daemon, Clippy and realPTY; compatible Icelake release191+6+4; isolated Linux two-UID24 concurrent requests, retained worker/PTY across gateway restart, cross-UID isolation and peer rejection; actual Apple→managed7 and rootless6 effective recovery tests, including multi-fragment missing-tail repair, epoch/resize fences and same-UUID reattach. CLI remains v2 and public N/N-1 is preserved. Internal ASTRAWK3 explicitly rejects old workers; deployment/rollback requires cold maintenance, not WORK-03 PTY migration.

The final archive adds no product changes. Only diff/protocol consistency, complete history/ancestor checks and bounded secret-pattern/artifact checks are repeated; no production test, restart, public release or release tag. No private keys, state archives, credentials, raw diagnostic captures or build products are committed. Check results are not a comprehensive security audit.

User feedback during unchanged running code reported substantial improvement. It does not identify the cause of the earlier two-second scroll tail. UI-18, WORK-03, dynamicPMTU/long-soak and untested mobile boundaries remain open. See the [cross-repository archive](https://github.com/astra-sec/astra-apple/blob/main/docs/architecture/terminal-optimization-archive-20261028.md).
