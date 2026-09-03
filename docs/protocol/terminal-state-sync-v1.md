# Terminal state synchronization v1

状态：`SYNC-01`、`SYNC-02`。本协议建立在 `terminal.semantic_state` v2 的单一服务端权威 State 与可靠 attachment stream 上，不启用 QUIC DATAGRAM，也不改变 Terminal State v2 schema。

## 协商

- `terminal.state_ack` v1 依赖 `terminal.semantic_state` v2。
- `terminal.semantic_diff` v1 同时依赖 `terminal.state_ack` v1 与 `terminal.semantic_state` v2。
- 未协商任一能力时不得试发对应消息；旧客户端继续接收可靠完整 State。

## 流程与窗口

1. attach 后服务端发送一份完整 `TerminalStateChunk`，并把该 `(epoch, generation)` 设为唯一在途状态。
2. Apple client 完成分片、SHA-256、protobuf、State v2 validator、Replica 原子应用和 renderer 导入后，发送 `TerminalStateAck`。
3. 在 ACK 到达前，服务端不导出或排队中间 State；所有 PTY 输出和 resize 只合并为一个 dirty bit。
4. ACK 到达且 dirty 时，服务端以最多 60 Hz 的 16 ms 间隔，从已 ACK State 直接生成到当前最新 State 的累计更新。
5. 每次仍只有一个 generation 在途。重复或更旧 ACK 被忽略，不会回退 base；不存在、未来或不匹配的 ACK 是协议错误。

该窗口把背压绑定到客户端真实的验证与渲染进度。慢客户端不会在可靠 QUIC stream 中积累数百份已经过时的完整网格；下一次 ACK 后直接收敛到最新权威 generation。

## 累计 diff

`TerminalStateDiff` 明确携带同一 epoch 的 `base_generation` 与 `target_generation`。`target_metadata` 是目标 State 的完整非行元数据，两个 Screen 的 `included_rows` 必须为空。`primary_rows` 和 `alternate_rows` 按目标顺序逐行指定：

- `base_anchor`：精确复用 base 中 anchor 和整行内容都未变化的 Row；
- `replacement`：携带新增或变化的完整 Row。

客户端按顺序重建两个 `included_rows`，拒绝缺失/重复 anchor、错误 base、跨 epoch、非递增 target 和任何目标 State v2 校验失败。只有完整目标原子应用并渲染后才 ACK。

服务端仍从当前权威 TerminalEngine 导出目标 State，不维护第二份终端模型。style/hyperlink 表来自完整目标 metadata；若表 ID 变化，受影响 Row 会作为 replacement 发送。diff 与快照都使用可靠 512 KiB 分片、16-byte transfer ID、总长度和 SHA-256，编码上限同为 8 MiB。

## 回退与边界

- epoch 变化、没有已 ACK base 或 diff 编码大小不小于完整 State：发送完整 State。
- broadcast lag：只标记 dirty；已 ACK base 到最新 State 的累计更新覆盖所有跳过事件。
- diff 不会跨 attachment、epoch 或 generation base 重用。
- `terminal.datagram_state` 仍未启用；`SYNC-03` 才能在保留可靠 fallback、MTU 和队列上限的前提下增加 DATAGRAM。
- N-1 decoder 会忽略新增 command/event oneof；capability 未选择时新消息不可达。
