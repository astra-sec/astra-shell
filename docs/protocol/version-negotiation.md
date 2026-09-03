# Astra application protocol negotiation

状态：`PROTO-01`。当前 application protocol `N = 2`，支持 `N-1 = 1`。QUIC ALPN 暂时保持 `astra/1`：ALPN 表示 framing/authentication family，Hello 中的 application version 表示可演进的消息语义。

## Hello 兼容规则

`ClientHello.protocol_version` 是为旧 server 保留的 exact-version hint。新 client 必须：

- 把 `protocol_version` 保持为 1，使只认识字段 1/2 的 v1 server 仍会接受；
- 用 `minimum_protocol_version`/`maximum_protocol_version` 广告真实范围，当前是 `1...2`；
- 以 `CapabilityOffer(name, min, max)` 广告已实现的能力范围。

新 server 收到没有 min/max 的旧 Hello 时，把 `protocol_version` 当成 exact range。收到新 Hello 时选择双方重叠范围的最高 application version。没有重叠必须在认证前失败。

`ServerHello.protocol_version` 是选择结果。server 只能返回 client offer 过的 capability，版本必须位于双方区间。client 必须原子校验整个选择；重复、未 offer、version 0、越界或未知选择均终止连接。

上限是 64 个 capability、名称 64 ASCII bytes。名称只能含小写字母、数字、`.`、`_`。server 忽略不认识的 client offer；client 不接受 server 凭空选择的能力。

## Capability registry

| 名称 | v1 语义 | 当前 runtime 是否 offer | 开启条件 |
|---|---|---|---|
| `terminal.legacy_ansi_snapshot` | 旧 `TerminalSnapshot` + ANSI replay | 是 | 迁移期保留 |
| `terminal.semantic_state` | 可靠分片传输的 `astra.terminal.v2.State`；禁止混发 raw PTY | 是（Apple client/server v2） | `TERM-03`、`TERM-04` 已完成；CLI 未实现 replica，因此只 offer legacy capability |
| `terminal.clipboard_write` | semantic attachment 上的 OSC 52 单向、受限剪贴板写事件；不包含读取 | 是（Apple client/server v1） | 必须同时选择 `terminal.semantic_state` v2；CLI 不 offer |
| `terminal.history_paging` | 使用 v2 `Anchor` 的可靠历史分页、trim 边界和客户端远端 viewport | 是（Apple client/server v1） | 必须同时选择 `terminal.semantic_state` v2；CLI 不 offer |
| `terminal.state_ack` | 客户端只在验证、应用并渲染完整 generation 后回 ACK；每个 attachment 最多一代在途 | 是（Apple client/server v1） | 必须同时选择 `terminal.semantic_state` v2；CLI 不 offer |
| `terminal.semantic_diff` | 从已 ACK generation 到最新 generation 的累计行级 diff；中间输出可合并 | 是（Apple client/server v1） | 必须同时选择 `terminal.state_ack` v1 与 `terminal.semantic_state` v2；CLI 不 offer |
| `session.objects` | Workspace CRUD、Terminal 归属和 Attachment 身份 | 是（Apple client/server v1） | 与 renderer capability 独立；CLI 保留 N-1 默认 Workspace 适配 |
| `terminal.input_lease` | controller lease TTL、renew/release 与 controller-only resize owner | 是（server v1；Apple 完成 SESS-02 后启用） | 依赖 `session.objects` v1；未协商客户端保留 Stream 生命周期 lease |
| `terminal.datagram_state` | generation 累计 patch 可走 QUIC DATAGRAM | 否 | `SYNC-01` 至 `SYNC-03` 完成 |

Capability 名称存在不代表实现完成。runtime support list 只能加入已经通过对应架构任务验收的能力；禁止为了让 UI 走新分支而提前 offer。

managed 模式下，gateway 在认证和协商完成后为每条 Unix worker stream 先发送内部 `WorkerStreamHello`。worker 必须按自己的 runtime support 重新验证 application version、capability 名称、版本、重复项和数量；gateway 同时加入认证连接 UUID，worker 校验其 canonical UUID 形式，网络 client 不能直接提供这份受信元数据。这样 rootless 和 managed attachment 使用同一份已验证 `NegotiatedProtocol` 和连接身份，不会由 `AttachRequest` 回显能力。

semantic attachment 的 `AttachResponse` 不携带 legacy snapshot。服务端紧接着发送一个或多个有序 `TerminalStateChunk`：16-byte transfer ID、chunk index/count、总大小和整份 State 的 SHA-256。当前每片上限 512 KiB、整份 State 仍受 8 MiB schema 上限约束，因此最多 16 片。Apple client 只有在顺序、元数据、总大小、SHA-256、protobuf decode 和 State v2 validator 全部通过后才发布一次状态；任何失败都终止 attachment，不发布半份状态。

同时选择 `terminal.state_ack` 与 `terminal.semantic_diff` 后，初始完整 State 是唯一无 base 的快照。客户端成功渲染后以 `(epoch, generation)` ACK；服务端在 ACK 前不发送下一代，只把任意数量的 PTY/resize 更新合并为一个 dirty 状态。ACK 后最多每 16 ms 从已确认 generation 直接构造到最新 generation 的累计 `TerminalStateDiff`。diff 以目标行顺序引用完全相同的 base row，只有新增或变化行携带完整 `Row`；其他 State/Screen metadata 与目标 style/hyperlink 表保持权威。客户端必须从精确 base 原子重建并再次运行完整 State v2 validator，成功渲染后才 ACK target。epoch 变化、base 不匹配或 diff 编码不小于完整 State 时，服务端使用可靠完整快照。详细不变量见 `terminal-state-sync-v1.md`。

history paging 只在 semantic v2 同时选择时生效。`HistoryPageRequest` 和 `HistoryPageChunk` 是 appended oneof；未选择能力的 N-1 decoder 会忽略它们。每页最多 512 rows/4 MiB，仍以可靠 512 KiB chunks 和整页 SHA-256 原子发布。

session objects 的 Workspace RPC 也是 appended oneof，所有资源身份字段只追加 tag。N-1 客户端映射到同一 SessionManager 的默认 Workspace；新客户端只在 capability 被选择后发送正式 RPC。详细边界见 `session-objects-v1.md`。

## N/N-1 行为矩阵

| Client | Server | 结果 |
|---|---|---|
| v2 | v2 | 选择 application v2，选择已实现 capability 交集 |
| v2 | v1 | v1 server 读取 legacy hint 1、忽略新增字段并返回 v1；v2 client 接受 v1、无 capability |
| v1 | v2 | v2 server 将旧字段解释为 exact v1，返回 v1、无 capability；v1 client 正常工作 |
| 低于 v1 / 高于 v2 且无重叠 | v2 | 认证前拒绝 |

测试必须同时覆盖 wire unknown-field 行为和语义选择，不能只测试 protobuf 能 decode。

## 字段弃用时间表

| 字段/路径 | v2 状态 | 停止发送条件 | 最早删除 wire tag |
|---|---|---|---|
| `AttachResponse.history` | deprecated fallback | `terminal.semantic_state` + `terminal.history_paging` 上线，所有受支持 client 已跨过 N-1 窗口 | application v4；删除时 reserve tag 4/name |
| `TerminalSnapshot.contents/normal_contents` | deprecated fallback | `TERM-03`、`TERM-04` 完成，v2 semantic capability 稳定一个 release | application v4；删除 message 前 reserve 所有 tag/name |
| `TerminalEvent.output` raw PTY | 仅未选择 semantic capability 的兼容路径 | N/N-1 迁移窗口结束，真实 TUI matrix 通过 | application v4；删除时 reserve tag 10/name |
| `TerminalEvent.snapshot` legacy ANSI | deprecated fallback | 同 `TerminalSnapshot` | application v4；删除时 reserve tag 14/name |

规则：一个字段在 N 标记 deprecated 后，至少完整支持 N/N-1 两个版本并经过一个发布窗口，才可在 N+2 停止发送；wire tag/name 最早在 N+2 schema 中 `reserved`，永不复用。实际删除还必须满足 `PROJECT_STATUS.md` 对应 PATCH/COMPAT 的移除条件，两者取更晚者。

## 安全与降级

- capability negotiation 在 TLS 保护的 QUIC stream 内、用户认证前完成；失败不得降级重试为猜测模式。
- 未选择 semantic capability 时只能走已登记的 legacy compatibility path，不能混发 v2 state。
- 未选择 `terminal.state_ack`/`terminal.semantic_diff` 时保持 v2 的可靠完整 State 路径；不得发送 ACK 或 diff 后再猜测降级。
- 未选择 DATAGRAM capability 时状态全部走可靠 stream；不能“试发再 fallback”。
- server 日志只记录 application version 和 capability 数量/名称，不记录终端内容、challenge、key 或 token。
