# Terminal datagram state v1

状态：Rust 数据面已实现；Apple 客户端尚未 offer。该能力优化 live viewport，不替代可靠控制流、Terminal State v2 或 Anchor 历史分页。

## 协商与路由

`terminal.datagram_state` v1 同时依赖：

- `terminal.semantic_state` v2；
- `terminal.state_ack` v1；
- `session.objects` v1；
- QUIC connection 提供非零 `max_datagram_size`。

每个 `TerminalViewportDatagram` 携带 canonical `terminal_id` 与当前 `attachment_id`。客户端在连接级读取 datagram，再按这对身份分发；未知或已注销 route 直接丢弃。每个 route 的接收邮箱只保留一个尚未消费的最新 datagram，新包覆盖旧包，不能使用无界 FIFO。因此一个认证 QUIC connection 可以复用任意多个 attachment，慢渲染器也不会积累旧画面，而 generation 仍只在各自 terminal epoch 内有意义。

managed 模式不会把 QUIC 交给 worker。gateway 在 `WorkerStreamHello.maximum_datagram_size` 中传递 payload 上限；worker 用内部 `TerminalEvent.viewport_datagram` 帧上送，gateway 再验证大小并调用 QUIC DATAGRAM。这个内部事件不得写入可靠 client stream。

## Keyframe 与 cumulative delta

attach 后服务端从权威 TerminalEngine 导出只含 primary/alternate 当前 viewport 的 State。primary scrollback 不进入 live keyframe，仍通过 Anchor 分页读取；不可见 style/hyperlink 表项同时过滤。初始 keyframe 使用可靠 `TerminalStateChunk`、整份 SHA-256 和 State v2 validator。客户端原子提交后发送 `TerminalStateAck`，在此之前服务端只合并 dirty，不发依赖该 base 的 datagram。

之后每份 datagram 都是从一个明确 retained base 直接到目标 generation 的 cumulative `TerminalStateDiff`，不依赖前一 datagram。相对 base 未变化的 styles、hyperlinks、modes、title、working directory 和 palette 由 `inherited_fields` 位标记继承，不能同时携带冲突值。客户端必须：

1. 验证 route、epoch、base 与单调 target；
2. 仅从精确 retained base 重建；
3. 恢复 inherited metadata；
4. 运行完整 State v2 与 viewport-only validator；
5. 原子替换 replica 后 ACK；
6. 丢弃旧代、重复代和旧 epoch datagram。

ACK 表示 validated replica 已提交，不表示 GPU 已完成绘制。ACK 只推进未来 delta base；服务端不会因尚未 ACK 当前 target 而停住下一代，所以渲染线程不能对网络读取施加逐帧可靠背压。

## 丢包、拥塞与收敛

- PTY/resize 更新以 16 ms 合并，最多约 60 Hz 导出最新状态。
- 任意中间 datagram 可丢失；较新的 cumulative delta 仍可从 retained base 独立重建。
- 旧包、重复包不能回退 current generation。
- 若最后一包丢失且输出停止，服务端每 100 ms 重发最新累计状态，收到 ACK 后停止。
- 若 encoded delta 超过当前 datagram payload 上限，不分片、不把旧代排进可靠队列，只保留最新 pending target。
- pending target 连续 1 s 没有 ACK 进展时提升为一份可靠 keyframe；健康的持续 ACK 会延后这个 deadline，epoch 变化则立即可靠 re-key。
- receiver 缺少 base 时立即发可靠 `TerminalStateRepairRequest(epoch, missing_base_generation, newest_seen_generation)`；server 校验范围后只发送最新 reliable keyframe。

QUIC DATAGRAM 自身受拥塞控制但不保证到达或顺序。可靠 input、resize、lease、detach、history 与 repair 仍走 attachment stream；丢失 screen generation 不会丢失用户命令。

- attachment 持有可写 lease 时，客户端在 TTL 的一半处通过可靠流自动续租；输入或 resize 前也会补做已到期的续租。read-only 或 lease 撤回后停止续租。
- Anchor history page 在可靠流上独立分片，接收端必须先验证 transfer ID、固定 metadata、总大小、SHA-256 与 HistoryPage validator，再一次性发布；未完成 transfer 不能被另一 transfer 静默替换。

## 上限与错误

- datagram 编码必须不大于 connection 当前 `max_datagram_size`；不得依赖固定 1200 字节假设。若 PMTU/peer limit 在 attach 后缩小，超限帧按本地丢包处理，attachment 保持存活并由 bounded reliable re-key 收敛。
- sender/receiver 默认最多保留 128 个 generation，并保护当前 cumulative base 不被驱逐。
- reliable State 与 history page 仍受各自编码上限、4096 chunks 和 SHA-256 完整性约束。
- route 不匹配、未知 inherited bit、冲突 metadata、未来 repair base、跨 epoch repair、非法 generation 或 validator 失败均为协议错误。
- capability 未选择时收到 datagram、repair 或内部 bridge event 必须拒绝，不能静默猜测降级。

## N-1

`WorkerStreamHello.maximum_datagram_size`、`TerminalCommand.state_repair` 与内部 `TerminalEvent.viewport_datagram` 都使用 appended protobuf tag。N-1 decoder 会保留已知外层字段并忽略新字段/oneof；新消息只有 capability 成功选择后才可达。
