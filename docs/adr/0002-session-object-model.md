# ADR 0002: Workspace、Terminal 与 Attachment 对象模型

- 状态：已接受
- 日期：2026-08-27
- 任务：SESS-01

## 背景

现有 daemon 直接以全局 `HashMap<String, Terminal>` 暴露 `List/Spawn/Attach`。所谓 Apple `AstraHostWorkspace` 只是本机 UI 容器；服务端没有 Workspace，Attachment 也只是某条 QUIC Stream 的隐含状态。这个结构无法可靠回答 Terminal 属于哪个资源容器、一个用户有多少 Attachment，因而不能作为 `OPS-01` 配额、`SESS-02` 租约或用户 worker 生命周期治理的基础。

## 决策

### 身份和所有权

```text
User Session
├── Workspace(UUID, name, revision, created_at)
│   └── Terminal(UUID, display_id, lifecycle, ProcessSpec, TerminalState)
└── Connection(UUID)
    └── Attachment(UUID, Workspace UUID, Terminal UUID, role, state)
```

- 所有持久或可重接对象使用随机 UUID；`display_id` 只供人工选择，不能用于客户端恢复。
- daemon 首次启动时创建一个持久的默认 Workspace。旧协议创建的 Terminal 归入该 Workspace。
- Terminal 必须引用一个存在的 Workspace；Terminal UUID 在用户 session 内全局唯一。
- Attachment 由服务端在 attach 请求成功时创建，绑定一次认证连接和一个 Terminal。重连创建新 Attachment UUID；writer `resume_token` 只恢复输入所有权，不复用 Attachment 身份。
- Attachment role 是 `VIEWER` 或 `CONTROLLER`。role 与输入 lease 分层：本 ADR 只建立身份和资源所有权；TTL/renew/release 由 `SESS-02` 实现。

### 生命周期

Terminal 的当前生产生命周期为：

```text
CREATING -> RUNNING -> EXITED -> DELETED
                |          ^
                +--close---+
```

`ARCHIVED` 保留为后续可选持久历史状态，不在 SESS-01 伪装实现。连接断开、detach 或客户端退出只使对应 Attachment 进入 `DETACHED` 并从活动注册表移除，不关闭 Terminal。

Attachment 的生产生命周期为：

```text
SUBSCRIBING -> SNAPSHOTTING -> LIVE -> DETACHED
                         ^       |
                         +--resync
```

活动注册表只保存尚未 `DETACHED` 的 Attachment；列表和配额不会依靠 QUIC Stream 数量猜测订阅关系。

### Workspace CRUD

- `List/Create/Rename/DeleteWorkspace` 是版本化 session RPC。
- Workspace 名称去除首尾空白后必须是非空 UTF-8，最多 128 bytes。
- 删除只允许空的非默认 Workspace；不会级联 kill 或移动 Terminal。
- Workspace catalog 使用版本化 protobuf 文件、临时文件 + `fsync` + 原子 rename 保存。Terminal/PTY 仍是运行时对象；本 ADR 不声称 daemon/主机重启后恢复进程。
- 新 Terminal 的 spawn 请求必须显式携带 Workspace UUID；list 以 Workspace UUID 为边界。

### 客户端布局边界

Workspace 是资源容器，不是 tmux window 或共享“当前 pane”。标签顺序、选中标签、窗格拆分、文件编辑位置和每设备可见性属于客户端布局文档；它们不得改变 Terminal 所属 Workspace，也不得由服务端强制所有设备同步。未来若同步布局，必须使用独立的版本化 layout document，而不是扩展 Terminal 或 Attachment 状态。

## 协议迁移

新增 `session.objects` v1 capability。协商该 capability 的客户端必须使用正式 Workspace RPC，并在 Terminal/Attachment 消息中发送和验证 Workspace/Attachment UUID。未协商 capability 的 N-1 客户端继续使用旧 `List/Spawn/Attach`：服务端将 spawn 映射到默认 Workspace、list 返回所有活动 Terminal，并在 attach Stream 内部创建真实 Attachment。该适配不拥有第二份会话状态，最早在 application v4 删除并 reserve 旧 oneof tags。

## 不在本 ADR 范围

- lease TTL、renew/release 和 resize owner（`SESS-02`）；
- 用户/全局资源上限（`OPS-01`）；
- 每用户 worker 的发现、故障和升级生命周期（`WORK-01/02/03`，见 ADR 0005）；
- 服务端同步客户端 layout；
- daemon 或主机重启后恢复 PTY。

## 验收

- Workspace catalog 能跨 SessionManager 重建保持 UUID、名称和 revision。
- Terminal 不能在不存在的 Workspace 中创建，且 list 不跨 Workspace 泄漏。
- 非空 Workspace 删除返回 conflict，不级联结束 Terminal。
- 每次 attach 返回唯一 Attachment UUID；命令必须匹配当前 Attachment，detach/断线后活动注册表归零。
- gateway 把认证连接 UUID 传到 user worker，worker 不信任网络客户端自报连接身份。
- Rust/Swift 均覆盖 capability 依赖、UUID/边界校验和 N/N-1 行为。
