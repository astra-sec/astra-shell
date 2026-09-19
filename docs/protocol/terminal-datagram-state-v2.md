# Terminal datagram state v2

状态：Rust 实验分支实现；通过 `astra --streaming` 显式启用；默认 CLI 和 Apple 客户端不 offer。该能力优化 live viewport，不替代可靠控制流、Terminal State v2 或 Anchor 历史分页。v1 为已撤回的实验格式，v2 不与 v1 协商互通；普通可靠协议不变。

## 协商与路由

`terminal.datagram_state` v2 同时依赖：

- `terminal.semantic_state` v2；
- `terminal.state_ack` v1；
- `session.objects` v1；
- QUIC connection 提供非零 `max_datagram_size`。

每个 `TerminalViewportDatagram` 携带 canonical `terminal_id` 与当前 `attachment_id`。客户端在连接级读取 datagram，再按这对身份分发；未知或已注销 route 直接丢弃。每个 route 的单槽邮箱仅接受已提交 keyframe 的 epoch，且 target generation 必须大于邮箱水位；乱序旧包不能覆盖未消费的新包。一个认证 QUIC connection 可在资源配额内复用多个 attachment；generation 仅在 terminal epoch 内有意义。

managed 模式不会把 QUIC 交给 worker。gateway 在 `WorkerStreamHello.maximum_datagram_size` 中传递 payload 上限；worker 用内部 `TerminalEvent.viewport_datagram` 帧上送，gateway 再验证大小并调用 QUIC DATAGRAM。这个内部事件不得写入可靠 client stream。

## Keyframe 与 cumulative delta

attach 后服务端从权威 TerminalEngine **直接**导出 primary/alternate 当前 viewport，不先构造 scrollback 再丢弃。样式表仅由可见单元格构建；相同引擎更新的 viewport 在多个 attachment 间复用缓存。历史仍通过 Anchor 分页读取。初始 keyframe 使用可靠 `TerminalStateChunk`、整份 SHA-256 和 State v2 validator。客户端原子提交后发送 `TerminalStateAck`，在此之前服务端只合并 dirty，不发依赖该 base 的 datagram。

之后每份 datagram 都是从一个明确 retained base 直接到目标 generation 的 cumulative `TerminalStateDiff`，不依赖前一 datagram。相对 base 未变化的 styles、hyperlinks、modes、title、working directory 和 palette 由 `inherited_fields` 位标记继承，不能同时携带冲突值。客户端必须：

1. 验证 route、epoch、base 与单调 target；
2. 仅从精确 retained base 重建；
3. 恢复 inherited metadata；
4. 运行完整 State v2 与 viewport-only validator；
5. 原子替换 replica 后 ACK；
6. 丢弃旧代、重复代和旧 epoch datagram。

v2 设置 `sparse_rows=true`，`diff.primary_rows/alternate_rows` 必须为空；两个 screen 的 `*_patches` 仅包含有变化的行。未列出的行继承同一 base index。每个 patch 含唯一的目标 index、base index、无 cells 的行 metadata，以及一次连续 cell splice (`cell_start/delete_count/cells`)；索引按语义 cell 数计，不是字节或显示列。可复用另一 base 行以表示滚动；宽字符、组合字符保持完整 cell。接收端拒绝越界、重复行号、混用两种行编码及非法重建结果。尺寸变化用 keyframe。

ACK 表示 validated replica 已提交，不表示 GPU 已完成绘制。ACK 只推进未来 delta base；服务端不会因尚未 ACK 当前 target 而停住下一代，所以渲染线程不能对网络读取施加逐帧可靠背压。

## 丢包、拥塞与收敛

- PTY/resize 更新以 16 ms 合并，最多约 60 Hz 导出最新状态。采样时间独立于发送时间：即便等待 ACK、仅更新 pending，也推进采样 deadline。
- 任意中间 datagram 可丢失；较新的 cumulative delta 仍可从 retained base 独立重建。
- 旧包、重复包不能回退 current generation。
- 若最后一包丢失且输出停止，服务端每 100 ms 重发最新累计状态，收到 ACK 后停止。
- encoded delta 超 MTU 或尺寸/epoch 变化时，若没有未确认 keyframe，立即发一份可靠 keyframe；否则只替换单个 pending target，绝不继续提交 keyframe。ACK 后从最新 pending 恢复。
- 可发送 datagram 的 pending target 连续 1 s 没有 ACK 进展时提升为 keyframe；健康 ACK 延后 deadline。100 ms retry 同样必须经过 keyframe gate，且每个发出的 generation 都进入可 ACK 记录。
- receiver 缺少 base 时发可靠 `TerminalStateRepairRequest(epoch, missing_base_generation, newest_seen_generation)`；server 校验后发送最新 keyframe。已有 keyframe 在途时合并重复 repair。
- 新 epoch 必须由可靠 keyframe 建立；旧 epoch 的已发送 ACK/repair 可以跨方向迟到，server 识别退休 epoch 后忽略，不能关闭 attachment。pending 新 epoch 不得被旧 ACK 清除。

QUIC DATAGRAM 自身受拥塞控制但不保证到达或顺序。可靠 input、resize、lease、detach、history 与 repair 仍走 attachment stream；丢失 screen generation 不会丢失用户命令。

可靠帧读取使用长寿命 `MessageReader` 保存部分 header/payload 进度；`select!` 或调用者取消后不会丢失帧边界。attachment 和 gateway 的可靠写任务独立于控制/bridge 接收循环；队列同时限制为 20 MiB、1024 个片段，超额则结束 attachment，而非无限占内存或阻塞续租。运行期间最多一份未 ACK keyframe；退出时允许额外一份最终 state，然后 `Exited`，这是有限的终止 flush。大画面仍受可靠链路带宽/RTT 限制，不承诺所有 TUI 都达到 60 fps。

- attachment 持有可写 lease 时，客户端在 TTL 的一半处通过可靠流自动续租；输入或 resize 前也会补做已到期的续租。read-only 或 lease 撤回后停止续租。
- Anchor history page 在可靠流上独立分片，接收端必须先验证 transfer ID、固定 metadata、总大小、SHA-256 与 HistoryPage validator，再一次性发布；未完成 transfer 不能被另一 transfer 静默替换。

## 上限与错误

- datagram 编码必须不大于 connection 当前 `max_datagram_size`；不得依赖固定 1200 字节假设。若 PMTU/peer limit 在 attach 后缩小，超限帧按本地丢包处理，attachment 保持存活并由 bounded reliable re-key 收敛。
- sender/receiver 默认最多保留 128 个 generation，并保护当前 cumulative base 不被驱逐。
- reliable State 与 history page 仍受各自编码上限、4096 chunks 和 SHA-256 完整性约束。
- route 不匹配、未知 inherited bit、冲突 metadata、未来 repair base、未知 epoch repair、非法 generation 或 validator 失败均为协议错误；已退休 epoch 的迟到控制消息不是错误。
- capability 未选择时收到 datagram、repair 或内部 bridge event 必须拒绝，不能静默猜测降级。

## Rust 使用与恢复

`StreamingAttachment` 是低层、由调用者驱动的协议 API。交互程序应使用 `StreamingConnection` / `LiveTerminal`：后台 driver 持续读取、验证、ACK、续租，向渲染器发布单槽最新 State。多个 LiveTerminal 共用一个受锁保护的连接；一次重新认证后其他 attachment 复用该连接，并各自用 workspace/terminal/resume token 恢复。每次失败有 125–250 ms 起、上限 5 s 的指数退避与 jitter，每次连接/attach 尝试最多 10 s。

状态为 Synchronizing → Live → Recovering → Synchronizing → Live；恢复后必须先收到新 keyframe。尺寸保留最新值，断线输入不缓存、不重放；命令带本地 incarnation，旧连接排队输入不得进入新连接。恢复不主动 takeover 别人的租约；明确的 not_found/权限错误终止，暂时的 quota/lease 冲突可退避重试。

`astra --streaming user@host` 新建交互终端；`astra --streaming user@host attach ID` 附着已有终端；Ctrl-] 分离。实验入口要求交互 TTY，管道输入继续使用默认 CLI。ANSI 仅作为本地渲染后端，按变化的可见行绘制，网络仍传 State/delta；SIGWINCH 发送尺寸更新。TTY 使用独立非阻塞描述符，退出时不会留下 Tokio 阻塞 stdin 读任务。原生历史分页 API 已支持，但实验 CLI 暂未提供独立 scrollback 浏览 UI。Swift 尚未迁移。

## N-1

`WorkerStreamHello.maximum_datagram_size`、`TerminalCommand.state_repair` 与内部 `TerminalEvent.viewport_datagram` 都使用 appended protobuf tag。N-1 decoder 会保留已知外层字段并忽略新字段/oneof；新消息只有 capability 成功选择后才可达。
