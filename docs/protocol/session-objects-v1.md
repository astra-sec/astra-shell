# Session Objects v1

`session.objects` v1 makes Workspace, Terminal and Attachment explicit resources. It is available on application protocol v2 peers and is independent of terminal rendering capabilities.

## Workspace

`WorkspaceInfo.id` is a canonical lowercase UUID. `revision` starts at one and increases after each metadata mutation. Exactly one Workspace has `is_default = true`; legacy spawn maps to it.

The RPC set is:

- `ListWorkspacesRequest` -> `WorkspaceListResponse`
- `CreateWorkspaceRequest` -> `WorkspaceResponse`
- `RenameWorkspaceRequest` -> `WorkspaceResponse`
- `DeleteWorkspaceRequest` -> `AckResponse`
- `ListTerminalsRequest` -> `TerminalListResponse`

Names are canonical trimmed UTF-8 without control characters and at most 128 bytes. Deleting the default Workspace or any Workspace that still owns a retained Terminal returns `conflict`; delete never cascades.

The server stores only container metadata in `session-catalog.pb` using schema version 1, a 1 MiB encoded upper bound, a mode-0600 temporary file, `fsync`, and atomic rename. PTYs remain runtime resources and are not claimed to survive a daemon or host restart.

## Terminal

When the capability is selected, `SpawnRequest`, `AttachRequest`, `CloseRequest` and `RenameTerminalRequest` carry `workspace_id`; the server rejects missing, noncanonical or mismatched ownership. `TerminalInfo.workspace_id` and `TerminalInfo.lifecycle` are required in formal responses.

The implemented lifecycle is `CREATING -> RUNNING -> EXITED -> DELETED`. `ARCHIVED` is reserved but is not produced yet. Disconnecting a transport does not change Terminal lifecycle.

## Attachment

Each successful attach creates a new canonical UUID and returns `AttachResponse.attachment`. The server owns `connection_id`; gateways forward it in `WorkerStreamHello`, and workers reject invalid connection metadata. An Attachment identifies one connection, Workspace, Terminal, role and active state.

Formal `TerminalCommand` messages must match both `terminal_id` and `attachment_id`. Every `TerminalEvent` carries the corresponding `attachment_id`; clients reject cross-attachment events. Input lease IDs remain a separate fencing layer and are extended by SESS-02.

Active state transitions are:

```text
SUBSCRIBING -> SNAPSHOTTING -> LIVE
                   ^            |
                   +------------+  resync
```

Detach, end-of-stream, connection loss and handler failure all remove the Attachment from the active registry through the same ownership guard.

## N/N-1

Unselected peers retain the old `List/Spawn/Attach` wire path. It maps into the same SessionManager and default Workspace; it does not maintain a parallel catalog or Attachment registry. New fields are additive, and new request/response oneof tags are ignored by N-1 decoders. The legacy session RPC boundary is registered as `COMPAT-03` and is eligible for removal no earlier than application v4.
