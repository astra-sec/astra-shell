# ADR 0003: 分层资源配额与容量预留

- 状态：已接受
- 日期：2026-08-27
- 任务：OPS-01

## 背景

现有代码只有零散常量：QUIC 每连接最多 128 条 bidi stream、文件服务最多跟踪 128 个 upload、Terminal 固定保留 2,000 行。这些常量没有共同所有者、没有用户与全局层级，也不能证明失败时不会结束已经运行的 Terminal。`HIST-03` 若直接扩大历史容量，会继续放大这个缺口。

SESS-01 已建立 Workspace、Terminal 和 Attachment 的正式所有权，因此资源配额可以在对象创建边界实施，而不再从进程名、Stream 数或 UI 状态猜测资源。

## 决策

### 统一资源维度

`ResourceClaim` 使用有符号安全的 `u64` 计量以下维度：

- 认证连接数；
- 活动 request/attachment Stream 数；
- managed user worker 数；
- Terminal/PTY 数；
- Attachment 数；
- Terminal 基础内存容量；
- Terminal history 容量；
- 活动文件操作/文件句柄容量；
- 活动 upload 数和声明的 upload bytes。

所有加法在改变计数前使用 checked arithmetic。任一维度超过上限时，整个 claim 原子失败并返回 `quota`；已经成功取得的上层 reservation 同步回滚。

### 两层 admission

```text
managed gateway
├─ global ResourcePool
├─ per-user ResourcePool
│    ├─ Connection reservation
│    ├─ Stream reservation
│    └─ Worker capacity bundle
│         ├─ N Terminal slots
│         ├─ Terminal memory/history bytes
│         ├─ Attachment slots
│         └─ file/upload capacity
└─ unprivileged user worker
     └─ assigned-capacity ResourcePool
          └─ actual Terminal/Attachment/file/upload reservations
```

managed gateway 不跨特权边界读取 Terminal 内容，也不等待 worker 上报每个 cell 的内存变化。启动或重新发现一个 user worker 时，gateway 从全局和该用户的 pool 中预留完整 user capacity bundle；worker 只在这块已分配容量内逐项 admission。因此：

- 全局最坏情况是所有已分配 worker bundle 之和，不会超过 global policy；
- 每用户只有一个 worker bundle，不能通过重复连接获得第二份容量；
- gateway 重启后首次重新使用已有 worker 时重新取得同样 bundle；降低配置不会杀死旧 worker，只会拒绝无法重新 admission 的新使用；
- worker 崩溃后 reservation 被释放，重启 worker 必须重新 admission。

rootless 模式没有特权分层，SessionManager、FileService、连接和 Stream 直接从同一 global + current-user 两层 pool 取得 reservation。

### 生命周期

reservation 是不可 clone 的 RAII guard，并由资源所有者持有：

| 资源 | reservation 所有者 | 释放时机 |
|---|---|---|
| Connection | 已认证 connection handler | 连接 handler 返回 |
| Stream | request/attachment task | task 结束、reset 或失败 |
| Worker bundle | WorkerRouter 的 UID registry | 已启动 worker 退出 |
| Terminal/PTY、memory、history | `Terminal` | Terminal 从活动/保留 registry 删除 |
| Attachment | `ActiveAttachment` | detach、EOF、连接失败或 handler error |
| File handle | file request/watch handler | 操作或 watch 结束 |
| Upload count/bytes | active `Upload` | commit、abort 或 registry 删除 |

构造 PTY、临时上传文件等有副作用的资源之前必须先 reservation；后续构造失败依靠 guard 自动回滚。配额只拒绝新资源，不选择或杀死活动 Terminal。

### Terminal 容量 claim

每个 Terminal admission 同时申请一个 PTY slot、一个 history budget 和基础 terminal memory budget。基础内存 claim 至少是 4 MiB，并随初始 `rows * cols` 增长；history claim 是分配给该 Terminal 的 8 MiB 容量。OPS-01 管理这些容量的分配和总量，`HIST-03` 必须让权威 TerminalEngine 按已分配 history bytes 和行数双重 trim；在此之前仍保持现有 2,000 行，不把容量 reservation 冒充新的历史实现。

### 配置和默认值

默认 policy 至少允许原计划的一连接 32 Terminal 验收：每用户 64 Terminal、256 Attachment、8 个连接和 256 条活动 Stream。每用户 terminal base memory 为 256 MiB，history 为 512 MiB；每个 Terminal 默认分别 claim 至少 4 MiB 和 8 MiB。文件侧允许 256 个活动 file operation、16 个活动 upload 和 8 GiB 声明 upload bytes。

全局默认允许 64 个 user worker、1,024 个认证连接和 8,192 条活动 Stream；Terminal、memory、history、file 和 upload 的全局容量必须至少容纳一个完整 user bundle。所有值由 `astrad serve` 参数覆盖；0 不是“无限”，配置必须显式合法。内部 worker 的 policy 只能由 gateway 启动参数提供，网络客户端不能声明或提高配额。

## 错误与兼容

- 超限使用稳定错误 code `quota`，message 指明资源、requested/current/limit；不得回退为 `spawn`、`filesystem` 或 transport reset。
- application protocol 不增加字段，N/N-1 客户端都能读取现有 `ErrorResponse`；因此本任务不新增 COMPAT 路径。
- Quinn transport 的 peer-advertised stream 上限仍是协议防线；应用 ResourceGovernor 是更严格的用户/全局 admission，二者不能互相替代。

## 不在本 ADR 范围

- `HIST-03` 的实际 heap byte 计量、10,000 行默认值和 byte-aware trim；
- `NET-02` 的 StreamHello 分类、公平调度和文件带宽整形；
- `OPS-02` 的 metrics/doctor/qlog；
- 认证前 Retry、速率限制和 DoS 防护（`SEC-01`）；
- OS/cgroup 磁盘与进程 RSS 强制限制。

## 验收

- 双层 reservation 任一层失败时计数不泄漏；drop、失败和并发竞争后 usage 回到基线。
- 第 N+1 个 Terminal、Attachment、Connection、Stream、file operation 或 upload 在副作用前返回 `quota`，已有对象保持可用。
- upload 按声明总大小 reservation，commit/abort 释放；相同 transfer ID 的 resume 不重复计数。
- managed worker bundle 使所有 user worker 的容量总和不超过 global policy，并保证同 UID 只占一个 bundle。
- rootless 与 managed worker 使用同一资源类型和失败语义。
- Rust 单元与集成测试覆盖逐维超限、原子回滚、RAII 释放、并发 admission 和 N/N-1 error decode。
