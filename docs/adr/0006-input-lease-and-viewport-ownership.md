# ADR 0006: 输入租约与 Terminal viewport 所有权

- 状态：已接受
- 日期：2026-08-28
- 任务：SESS-02
- 依赖：SESS-01

## 背景

现有 Terminal 只有 connection-scoped `lease_id` 与 writer `resume_token`。租约在 attach
Stream 结束时释放，也可由 takeover 替换，但没有 TTL、续租或显式释放。一个失去网络但
尚未被服务端观察到 EOF 的 controller 会无限占有输入权；客户端也不能在进入后台或主动
放弃控制时及时降级。Resize 复用输入 lease fencing，但协议没有正式声明 controller 是
唯一 PTY viewport owner。

Apple 已完成 presentation host 与本地 viewport generation：native bounds 是客户端几何
权威，semantic State 不能反向覆盖它。本 ADR 补齐服务端领域和 wire 边界，不引入第二套
screen、延时 resize 或 UI 生命周期特判。

## 决策

### 能力与兼容

新增 `terminal.input_lease` v1 capability，依赖 `session.objects` v1。只有协商该能力的
attachment 使用 TTL、renew 和 explicit release；N-1 客户端继续使用 attach Stream
生命周期租约，不会因不认识续租消息而被自动过期。能力选择后：

- writable `AttachResponse` 返回非空 `lease_id`、`resume_token` 和 `lease_ttl_ms`；
- read-only attachment 三者均为空或零；
- `TerminalCommand.lease_control` 只接受 `RENEW` 或 `RELEASE`；
- input、resize、renew、release 都必须同时匹配 Terminal、Attachment 和当前 lease ID。

默认 TTL 为 15 秒；客户端应在剩余 TTL 的约三分之一处（默认每 5 秒）续租。TTL 是
duration，不使用客户端与服务端墙钟比较。续租通过可靠有序 attachment Stream 发送，
不会重置 command sequence fencing。

### 状态与 fencing

每个 Terminal 最多一个 controller lease：

```text
none -> granted -> renewed* -> released
                  |       \-> expired
                  \----------> taken_over / resumed（旧 lease revoked，新 lease ID）
```

- 每次 grant、takeover 或 resume 都生成新的随机 `lease_id`；resume token 只能授权恢复
  writer 身份，不能复用旧 fencing ID。
- renew 只延长完全匹配的当前 lease；旧 attachment 的 delayed renew/release/expiry 不得
  改变新 lease。
- release 和 expiry 原子清除当前 lease，并把对应 Attachment role 降为 viewer。
- takeover/resume/expiry 通过 `LeaseChanged` 通知旧 controller；reason 是稳定枚举语义，
  旧 `reason` 字符串仅为兼容显示。
- lease 过期后 input 与 resize 都返回稳定 lease error，不得静默接受或缓存重放。

### Viewport owner

持有当前 controller lease 的 Attachment 是服务端唯一 PTY viewport owner。只有它可以
发送 resize；viewer/observer 永远不能改变 Terminal rows/cols。controller 的客户端
presentation host 仍是本机 viewport measurement 的唯一 owner，服务端只接受经过
Attachment + lease + sequence fencing 的测量结果。

lease release、expiry 或 takeover 不会把 PTY 自动 resize 到 observer，也不会从最近
State 推断尺寸。新 controller 在获得 lease 后发布其当前有效 host measurement；在此之前
Terminal 保持最后一次已接受的尺寸。

### 客户端生命周期

- mounted、visible、window-active 与平台前后台状态只控制 responder 和是否主动持有/续租；
  不创建新 TerminalView，也不伪造 viewport generation。
- writable attachment 在可服务生命周期内自动 renew；主动 detach、切换为 read-only 或
  生命周期明确结束时先 best-effort release，再关闭 Stream。
- 短暂后台不立即销毁 attachment；停止续租后由 TTL 提供有界回收。恢复时若租约已失效，
  客户端重新 attach/read-only 或显式 takeover，不反复调用 responder。
- 本地进程与串口继续复用 presentation lifecycle，但不发送远端 lease 控制消息。

## 验收

- Rust 领域测试覆盖 grant、renew、release、expiry、resume/takeover rotation、旧 lease
  delayed cleanup fencing 与 viewer resize 拒绝。
- wire/N/N-1 测试证明新 capability 依赖、未知字段兼容和旧客户端 connection-scoped 行为。
- macOS/iOS 客户端测试覆盖自动续租、显式释放、read-only/takeover、后台超过 TTL 后恢复、
  重连换 Attachment，以及旧 attachment/generation 不能发送 input 或 resize。
- AppKit/UIKit 真实生命周期验证覆盖新建、窗口最大化/缩放、标签切换、后台恢复和重连；
  本地/串口终端仍不发送 lease 消息。
