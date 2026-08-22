# 基于 QUIC 的持久多通道远程终端系统：完整实现计划

> 状态：架构提案 v0.1
> 日期：2026-08-21
> 暂定项目名：QTerm（仅用于本文指代）

## 1. 执行摘要

本项目不是把 QUIC、Mosh 和 tmux 简单拼接，而是实现一个新的远程终端平台：

- 每个主机/用户维持一个 QUIC 连接；
- 在该连接内承载多个相互独立的 PTY、一次性命令、文件和端口转发通道；
- 远端守护进程持有 PTY，客户端退出、休眠或网络中断时进程继续运行；
- 客户端把远端终端直接呈现为应用原生标签、窗格和窗口，不再显示 tmux 的文本 UI，也不要求用户学习 tmux 前缀键；
- 交互式终端使用类似 Mosh 的“最终状态收敛”机制，在丢包时抛弃过期画面；
- QUIC 负责 TLS 1.3、拥塞控制、丢包恢复、多流、路径验证和连接迁移，不再自行实现通用 UDP 传输层；
- QUIC 被阻断时，使用同一应用协议经 SSH stdio 退化运行。

三个现有技术分别被替换或吸收如下：

| 现有部分 | 保留的思想 | 被替换的部分 |
|---|---|---|
| QUIC | 单 UDP 端口、多 Stream、DATAGRAM、连接迁移、TLS 1.3 | 不自行实现加密、重传、拥塞控制和 NAT 漫游 |
| Mosh | 累计状态差分、旧状态可丢弃、客户端预测、认证后迁移 | 单 Shell/单 UDP 端口模型、自制网络传输与密码层 |
| tmux | 长期 server 持有 PTY、screen/grid、滚动缓冲、detach/attach、每客户端背压 | 文本分屏 UI、前缀键、控制模式文本协议、布局与 PTY 强耦合 |

推荐技术路线为：**Rust + Tokio + Quinn + rustls + Prost/Protobuf + 可替换终端引擎接口**。首个版本以 Linux/macOS 服务器和 Windows/macOS/Linux 客户端为目标；服务器端 Windows/ConPTY、移动端和系统级多用户网关放到后续阶段。

完整产品不是一个小型周末项目。以 5 名有相关经验的工程师计算，达到可公开试用约需 6 个月，达到经过安全审计、可用于生产约需 10–12 个月；单人实现更现实的时间是 18–24 个月。

## 2. 源码调研基线与结论

本文直接阅读了以下当前主干版本：

- Mosh：`decd9b705eb81626f694335b8d5940538beb06da`（2026-03-22）
- tmux：`8dfa9033467ed6ec643a49b0f35cc99ac456ba02`（2026-08-20）
- Quinn：`b0afb6e6fb2261ee1c6a1f69f396627e80ff4fb7`（2026-08-20）

### 2.1 Mosh 中应继承的设计

Mosh 的状态同步协议不是传统可靠字节流。其传输指令包含 `old_num`、`new_num`、`ack_num`、`throwaway_num` 和 `diff`，见 [`transportinstruction.proto`](https://github.com/mobile-shell/mosh/blob/decd9b705eb81626f694335b8d5940538beb06da/src/protobufs/transportinstruction.proto)。发送端基于“推测接收端当前状态”计算累计差分，并维护一个有界状态队列；丢掉中间包后，后续差分仍可直接从最后确认状态收敛到最新状态，见 [`transportsender-impl.h`](https://github.com/mobile-shell/mosh/blob/decd9b705eb81626f694335b8d5940538beb06da/src/network/transportsender-impl.h)。

值得继承的具体机制：

1. **状态代次而非字节偏移**：终端输出被建模为状态 `S_n`，更新是 `diff(S_ack, S_latest)`。
2. **累计差分**：若中间画面丢失，下一帧仍从客户端最后确认状态计算。
3. **发送合并**：Mosh 大约以每 RTT 两帧发送，并设置上下限；新实现只保留渲染合并，不重复实现拥塞控制。
4. **认证包触发漫游**：Mosh 只在解密、方向和序列号验证成功后更新客户端地址，见 [`network.cc`](https://github.com/mobile-shell/mosh/blob/decd9b705eb81626f694335b8d5940538beb06da/src/network/network.cc)。QUIC 已提供更完整的 Connection ID 与路径验证。
5. **服务端终端模型**：`Terminal::Complete` 将 PTY 输出解析到 framebuffer，再生成目标终端的最小显示差分，见 [`completeterminal.cc`](https://github.com/mobile-shell/mosh/blob/decd9b705eb81626f694335b8d5940538beb06da/src/statesync/completeterminal.cc)。
6. **保守预测**：客户端预测以 overlay 存在，预测和已确认 framebuffer 分离；批量粘贴会禁用预测，迟迟未确认的预测会加下划线。

不应直接继承：

- Mosh 的网络重传、RTT、拥塞提示和 AEAD 层；这些由 QUIC 负责。
- 一会话一个 `mosh-server`、一个 UDP 端口和一个 Shell 的模型。
- 仅同步可见 framebuffer、滚动缓冲不完整的限制。
- GPLv3 源码本身，除非项目决定整体采用 GPLv3。若希望使用 Apache-2.0/MIT，应基于公开协议思想和独立测试重新实现，不复制 Mosh 代码。

### 2.2 tmux 中应继承的设计

tmux 的长期 server 通过 Unix socket 接受本地客户端；一个 server 管理所有 session、window 和 pane。每个 pane 由 `fdforkpty` 创建独立 PTY 和子进程，见 [`spawn.c`](https://github.com/tmux/tmux/blob/8dfa9033467ed6ec643a49b0f35cc99ac456ba02/spawn.c)。

关键数据结构集中在 [`tmux.h`](https://github.com/tmux/tmux/blob/8dfa9033467ed6ec643a49b0f35cc99ac456ba02/tmux.h)：

- `session`：命名、环境、cwd、窗口链接和当前窗口；
- `window`：pane 集合、活动 pane 和布局树；
- `window_pane`：PTY fd/pid、尺寸、解析器、screen/grid、滚动缓冲和输出偏移；
- `screen/grid`：终端单元格、光标、模式、主/备用屏幕和历史行。

PTY 输出的处理顺序很重要：

1. 从 PTY 的 bufferevent 读取新字节；
2. 给 control client 记录独立输出 offset；
3. 将字节解析进服务端 screen/grid；
4. 对慢客户端暂停或丢弃其订阅，而不是阻塞 PTY，见 [`window.c`](https://github.com/tmux/tmux/blob/8dfa9033467ed6ec643a49b0f35cc99ac456ba02/window.c) 与 [`control.c`](https://github.com/tmux/tmux/blob/8dfa9033467ed6ec643a49b0f35cc99ac456ba02/control.c)。

tmux control mode 已证明“一个服务端复用器向 GUI 暴露多个 pane”可行，但其文本协议有大量隐含语义，命令结果、异步通知和 `%output` 必须严格排序。新协议应保留事件模型、客户端独立 offset、每 pane 背压和公平调度，但使用强类型二进制消息。

不应继承：

- server 同时拥有 PTY、文本窗口管理、边框、状态栏和按键前缀的耦合；
- session/window/pane 的展示语义强制所有客户端共享同一“当前窗口”；
- 文本 control mode 及八进制转义；
- 以最小/最大客户端尺寸决定 pane 尺寸导致的多客户端显示问题；
- 单个 server 进程崩溃导致所有 PTY 一起受影响。

### 2.3 QUIC/Quinn 中直接使用的能力

QUIC v1 提供可靠有序 Stream、流控、多流、TLS 1.3、连接迁移和路径验证；DATAGRAM 扩展提供同一安全连接内的不可靠消息。实现不应再次构造一套 Mosh 式通用 UDP 可靠层。

Quinn 当前代码直接提供：

- `open_bi/open_uni` 与 `accept_bi/accept_uni`；
- `send_datagram/read_datagram`；
- `Endpoint::rebind`，用于客户端切换本地 UDP socket；
- 服务端迁移开关与远端地址变化观察；
- 0-RTT、连接统计、RTT、qlog；
- SendStream 优先级和同优先级 round-robin 公平调度；
- DATAGRAM 队列满时丢弃较旧待发数据，正适合“最新状态优先”。

相关源码见 [`connection.rs`](https://github.com/quinn-rs/quinn/blob/b0afb6e6fb2261ee1c6a1f69f396627e80ff4fb7/quinn/src/connection.rs)、[`endpoint.rs`](https://github.com/quinn-rs/quinn/blob/b0afb6e6fb2261ee1c6a1f69f396627e80ff4fb7/quinn/src/endpoint.rs) 和 [`datagrams.rs`](https://github.com/quinn-rs/quinn/blob/b0afb6e6fb2261ee1c6a1f69f396627e80ff4fb7/quinn-proto/src/connection/datagrams.rs)。

QUIC 不提供：

- 操作系统用户身份和 shell 授权；
- PTY 生命周期和客户端退出后的会话持久化；
- 终端解析、屏幕状态和滚动缓冲；
- 应用级通道类型、文件 API、会话恢复和多客户端输入仲裁。

## 3. 产品目标、范围与非目标

### 3.1 v1 必须满足

1. 同一主机/用户的一条 QUIC 连接承载至少 32 个同时活动的终端。
2. 每个终端对应独立 PTY、进程、输入序列、输出状态和流控。
3. 客户端网络地址变化时连接可迁移；迁移失败时新建连接并重新 attach，不杀死 PTY。
4. 客户端进程退出后重新启动，可以恢复终端可见状态和滚动缓冲。
5. 客户端直接展示标签/窗格，不要求安装或操作 tmux。
6. 一个高吞吐终端或文件传输不得无限期饿死交互输入与其他终端。
7. 默认 rootless 部署，普通 SSH 用户可自行安装。
8. UDP 被阻断时，可以在同一 SSH stdio 连接上运行相同应用协议。
9. 支持 bash、zsh、fish；服务端首期支持 Linux/macOS PTY。
10. 提供稳定 CLI 和可嵌入 client-core；桌面 GUI 可随后独立迭代。

### 3.2 完整产品目标

- Windows/macOS/Linux 桌面客户端；
- iOS/Android 客户端；
- 原生标签、窗格、工作区和布局恢复；
- 文件浏览、上传、下载、编辑和断点续传；
- TCP/UDP 端口转发；
- 一次性命令与后台任务；
- 每设备密钥、硬件密钥和可选凭据同步；
- 系统级多用户 `qtermd`，一台主机只开放 UDP/443；
- 只读协作、审计和受控输入接管。

### 3.3 明确非目标

- v1 不恢复服务器重启前的任意进程。客户端断开与服务器 daemon 重启可恢复；操作系统重启后的进程检查点属于 CRIU/虚拟机领域。
- 不兼容 Mosh wire protocol。
- 不复制 tmux 配置、插件、键表和所有命令语义。
- v1 不允许多个客户端无仲裁地同时写入同一 PTY。
- 不将 QUIC 0-RTT 用于执行命令、写文件、发送按键或杀死会话。
- 不自行实现密码算法、TLS、拥塞控制或 QUIC。

## 4. 总体架构

```text
┌────────────────────────── Client device ──────────────────────────┐
│ Desktop/Mobile UI                                                  │
│   native tabs/panes/workspaces                                     │
│             │                                                      │
│ qterm-client-core                                                  │
│   protocol · terminal replica · prediction · reconnect · cache     │
│             │                                                      │
│ Quinn QUIC endpoint                                                │
└─────────────┼──────────────────────────────────────────────────────┘
              │ one QUIC connection / host+user
              │ control streams + data streams + DATAGRAM
┌─────────────▼────────────────── Remote host ───────────────────────┐
│ qterm-sessiond (per user)                                          │
│   authz · workspace registry · attachments · quotas · state sync   │
│        │                    │                    │                  │
│  terminal-worker A       worker B             worker C             │
│  PTY + VT state          PTY + VT state       PTY + VT state       │
│        │                    │                    │                  │
│      zsh                  nvim                 build process        │
│                                                                    │
│ optional qterm-gatewayd: shared UDP/443 + multi-user routing       │
└────────────────────────────────────────────────────────────────────┘
```

### 4.1 组件职责

#### qterm-client-core

- 维护主机连接、认证和能力协商；
- 为所有终端分发 control event、Stream 和 DATAGRAM；
- 保存每终端 `epoch/generation/scrollback_line_id`；
- 应用状态补丁、请求快照、缓存离线布局；
- 管理输入租约和预测 overlay；
- 在路径迁移、QUIC 重建和 SSH fallback 之间切换。

#### qterm-sessiond

- 每个 Unix 用户一个长期进程；
- 管理 workspace、terminal、attachment 和持久元数据；
- 创建 terminal-worker，并在 client detach 后继续持有它；
- 为每个 attachment 维护已确认 generation 和订阅状态；
- 实施配额、授权、输入租约和清理策略；
- 不绘制 tmux 状态栏、边框或文本布局。

#### terminal-worker

- 每个 PTY 一个独立进程，降低单点崩溃影响；
- 拥有 PTY master、子进程 PID、终端解析器、主/备用 screen、滚动缓冲和原始输出环形缓冲；
- 通过权限为 `0600` 的 Unix socket 向 sessiond 注册；
- sessiond 重启后重新注册，因此 sessiond 崩溃不关闭 PTY master；
- 终端结束后保留退出状态和有限历史，按 TTL 清理。

MVP 可先将 worker 作为 sessiond 内部 task；在公开测试前拆成独立进程。

#### qterm-gatewayd

- 后续的系统级可选组件；
- 监听一个 UDP/443；
- 执行 QUIC Retry、TLS、认证、速率限制和用户路由；
- 使用 privilege-separation helper 启动对应用户的 sessiond；
- 不直接拥有 PTY，也不解析终端数据。

### 4.2 核心对象模型

```text
User
└── Workspace (UUID, name, metadata, optional layout)
    ├── Terminal (UUID, display_id, epoch, ProcessSpec, PTY, TerminalState)
    ├── Terminal
    └── Task/Exec

Connection
└── Attachment (UUID, Terminal UUID, role, ack_generation, viewport)
```

- `Workspace` 是命名容器，不等同于 tmux 的“当前窗口”。
- `Terminal` 是持久 PTY 和进程，是最核心对象。
- `Layout` 是客户端展示元数据；服务端可保存，但不能决定所有客户端必须显示哪个 pane。
- `Attachment` 表示某客户端对某 Terminal 的一次观看/控制关系。
- 所有持久对象使用随机 128 位 UUID。Terminal 另有由每用户 sessiond/worker 分配、生命周期内只增不复用的 `u64 display_id`，仅供 CLI 和界面人工选择；客户端缓存与自动恢复始终使用 UUID。连接内仍可协商短 `u32 handle`，避免每个 DATAGRAM 携带 UUID。

### 4.3 生命周期

Terminal 状态：

```text
CREATING → RUNNING → EXITED → ARCHIVED → DELETED
             │          │
             └─ signal ─┘
```

Attachment 状态：

```text
SUBSCRIBING → SNAPSHOTTING → LIVE → DETACHED
                    ▲          │
                    └─ resync ─┘
```

连接断开只销毁 Attachment，不销毁 Terminal。Terminal 仅在显式关闭、进程退出后 TTL 到期或用户配额策略触发时删除。

## 5. QUIC 传输设计

### 5.1 连接与 ALPN

- ALPN：`qterm/1`；
- TLS 1.3 由 rustls/Quinn 提供；
- 每个 `host + OS user + security realm` 一条连接；
- 服务端启用迁移，客户端网络变化时调用 `Endpoint::rebind`；
- QUIC idle timeout 建议 120 秒。长时间后台休眠后允许连接超时，唤醒时重建并 attach；不要用永久 keepalive 对抗移动系统；
- 前台有活动 attachment 时每 15 秒保活；后台不承诺保活；
- 开启 DATAGRAM，协商保守的 1200 字节应用上限，运行时读取 path MTU；
- 初期使用 Quinn 默认 CUBIC/NewReno 组合；BBR 只作为实验配置，不作为首发默认；
- 开启 Quinn send fairness，并设置少量明确优先级。

### 5.2 Stream 分类

| Stream | 方向 | 可靠性 | 优先级 | 用途 |
|---|---|---:|---:|---|
| Control | 双向 | 可靠 | 100 | Hello、认证、会话管理、事件、租约 |
| Terminal Input | 客户端→服务端 | 可靠 | 90 | 按键、粘贴、resize、signal |
| Snapshot/Patch | 服务端→客户端 | 可靠 | 80 | 初始快照、超大补丁、重同步 |
| Raw Output | 服务端→客户端 | 可靠 | 60 | 调试/兼容模式，不作为状态模式默认路径 |
| Exec | 双向 | 可靠 | 50 | 一次性命令的 stdin/stdout/stderr |
| File | 双向 | 可靠 | 10 | 文件读写、目录、断点续传 |
| Forward | 双向 | 可靠 | 20 | TCP 转发；每个转发连接一个 Stream |
| Terminal State | DATAGRAM | 不可靠 | 高交互 | 小型累计屏幕补丁与 ACK |

Quinn 已支持 SendStream priority；仍需应用层对文件通道设置速率上限，避免共享 congestion window 时吞噬交互容量。

### 5.3 Stream 开头的类型声明

每条新 Stream 首帧必须是：

```protobuf
message StreamHello {
  uint32 protocol_version = 1;
  StreamKind kind = 2;
  uint32 channel_handle = 3;
  uint64 terminal_epoch = 4;
  bytes request_id = 5;
}
```

不能把 QUIC Stream ID 当作持久通道 ID；断线重连后 Stream ID 会变化，而 Terminal UUID/epoch 不变。

### 5.4 0-RTT 规则

0-RTT 有重放风险。默认只允许以下幂等、只读操作：

- `ClientHello`；
- `ListWorkspaces/ListTerminals`；
- 带上次 generation 的只读 attach 意图，但在 1-RTT 确认前不授予输入租约。

以下操作必须等待 `Connection::authenticated()`/1-RTT：

- 创建或关闭 Terminal；
- PTY 输入、粘贴和信号；
- 文件写入、删除和 rename；
- 端口转发；
- 输入租约接管；
- 修改配置和权限。

### 5.5 连接中断与恢复

恢复分两级：

1. **QUIC path migration**：客户端进程和连接状态仍在，仅本地地址变化；连接及 Stream 保持。
2. **应用级 reattach**：客户端进程被杀、QUIC idle timeout、服务器网络重启；新建 QUIC 连接后发送 `ResumeManifest`，列出每个 Terminal 的 epoch、已确认 generation 和滚动缓冲 line ID。

服务端对每个 attachment 决定：

- generation 仍可累计：发送差分；
- generation 已淘汰：发送完整快照；
- epoch 不一致：终端已重启，发送 `TerminalReplaced` 和新快照；
- Terminal 已退出：发送最终状态和 exit status；
- Terminal 不存在：返回明确 tombstone，而不是静默新建。

### 5.6 UDP 被阻断时的 fallback

应用协议必须建立在抽象的 `FramedTransport` 上，而不是在业务层直接依赖 Quinn：

```rust
trait FramedTransport {
    async fn open_reliable_channel(&self, kind: StreamKind) -> Channel;
    async fn send_latest_state(&self, frame: StateFrame) -> Result<()>;
    async fn recv_event(&self) -> Result<Event>;
}
```

实现：

- `QuicTransport`：Streams + DATAGRAM；
- `SshStdioTransport`：一次 SSH exec `qterm-sessiond stdio`，在一条可靠字节流上应用级复用；状态更新退化为可靠消息，仍保留 sessiond 持久性；
- 后续可增加 WebSocket/HTTP CONNECT fallback。

## 6. 应用协议

### 6.1 编码与版本

- Control 和 Stream framing：QUIC varint 长度 + Protobuf（Rust 使用 Prost）；
- Snapshot：版本化二进制 cell-grid + zstd，外层带未压缩长度上限；
- DATAGRAM：紧凑固定头 + varint 字段，不使用通用 JSON；
- Protobuf 字段只追加，删除字段必须 `reserved`；
- 每个请求有 128 位 `request_id`，服务端维护短期幂等缓存；
- 错误分为 `transport`, `auth`, `quota`, `not_found`, `conflict`, `terminal`, `filesystem`, `unsupported`。

### 6.2 Control 消息

首版至少包含：

- `ClientHello/ServerHello/CapabilitySet`；
- `Authenticate/AuthResult`；
- `List/Create/Rename/DeleteWorkspace`；
- `List/Spawn/Attach/Detach/CloseTerminal`；
- `Acquire/Renew/ReleaseInputLease`；
- `ResizeTerminal/SendSignal`；
- `TerminalCreated/Exited/Replaced/MetadataChanged`；
- `StateAck/SnapshotRequest/ScrollbackRequest`；
- `ResumeManifest/ResumePlan`；
- `Ping/Pong/ServerDraining/Error`。

### 6.3 DATAGRAM 格式

连接建立后，Control Stream 将 Terminal UUID 映射为短 handle。状态包建议格式：

```text
version:u8 | kind:u8 | flags:u16
channel_handle:varint
terminal_epoch:varint
base_generation:varint
target_generation:varint
payload...
```

DATAGRAM 不做跨包无限重组。若补丁超过当前 `max_datagram_size`：

- 优先拆成彼此独立、可丢弃的绝对 row patch；
- 无法安全拆分时改走可靠 Snapshot/Patch Stream；
- 禁止在内存中无上限等待 fragment，避免放大攻击。

## 7. Mosh 式终端状态同步

### 7.1 权威状态

服务端 terminal-worker 是权威终端模拟器：

- 读取 PTY 原始输出；
- 更新 main/alternate screen、光标、模式、颜色、标题、超链接和滚动缓冲；
- 对每次可观察变化增加 `generation`；
- 对每行记录 `last_modified_generation`；
- 滚动缓冲使用单调 `line_id`；
- 保存最近若干快照或生成差分所需的行版本。

客户端维护相同的语义模型，但不负责决定权威状态。

### 7.2 累计差分算法

对 attachment `A`：

```text
acked_generation[A] = g
current_generation = n

patch = diff(S_g, S_n)
send patch(base=g, target=n)
```

若该 patch 丢失，客户端仍 ACK `g`，服务端下一次直接生成 `diff(S_g, S_m)`。不重发过期的 `g→n` 字节包。

补丁操作应尽量是绝对、幂等操作：

- `ReplaceRows(start, rows[])`；
- `SetCursor(row, col, style, visible)`；
- `SetModes(...)`；
- `SetTitle/SetCwd`；
- `AppendScrollback(first_line_id, lines[])`；
- `TrimScrollback(before_line_id)`；
- `Bell/ClipboardRequest/ImageUpdate` 等独立事件。

客户端只有在 `base_generation == local_generation` 时应用相对补丁。否则丢弃并发送当前 ACK；服务端将产生累计补丁或可靠快照。

### 7.3 快照

快照通过可靠 Stream 发送，包含：

- Terminal UUID 和 epoch；
- generation；
- 尺寸与像素信息；
- main/alternate screen；
- 光标和 terminal modes；
- 颜色、标题、cwd、超链接表；
- 有界滚动缓冲范围及 line ID；
- capability-dependent 扩展。

快照必须：

- 有未压缩大小上限；
- 可中途取消；
- 在内存中流式解压；
- 校验 schema/version 和所有长度；
- 完成后原子替换客户端 replica，避免半快照状态。

### 7.4 更新节奏

- 交互输入后的最小合并延迟：4–8 ms；
- 常规上限：60 fps；移动端默认 30 fps；
- 高 RTT 时可参考 Mosh 的 `SRTT/2`，但只用于 UI 合并节奏；
- Quinn 负责拥塞控制，应用层不能再维护独立 congestion window；
- DATAGRAM 队列满时允许丢弃旧状态，只保留最新累计目标；
- 每个 attachment 定期 ACK，且在焦点终端上更积极，后台终端降低帧率。

### 7.5 输入与预测

输入必须走可靠、有序 Stream：

```protobuf
message TerminalInput {
  uint32 channel_handle = 1;
  uint64 terminal_epoch = 2;
  uint64 input_seq = 3;
  bytes data = 4;
  InputKind kind = 5; // key, paste, ime_commit
}
```

预测作为后续功能，采用与 Mosh 相同的 overlay 原则，而不是直接修改已确认 replica：

- 仅在 RTT 超过阈值时自适应开启；
- 仅预测普通可打印字符、Backspace、左右移动和简单行编辑；
- paste、密码输入、IME composition、alternate screen、鼠标模式和未知 TUI 默认禁用；
- 预测单元格使用视觉标记；
- 服务端返回 `input_seq` 的 applied/echo watermark；
- 若权威状态与预测不同，立即回滚 overlay；
- 不以“写入 PTY 成功”冒充“终端已正确显示”，两者必须区分。

### 7.6 终端查询与客户端能力

服务端终端引擎必须处理应用对终端的查询：DA/DSR、颜色、窗口尺寸、像素尺寸、剪贴板和图形能力。原则：

- 固定协议能力由服务端直接回答；
- 与物理客户端相关的值取当前 input-lease owner；
- OSC 52 等敏感操作转换为客户端事件，并由本地策略批准；
- 图片、超链接和剪贴板都有独立大小/频率限制；
- 未支持的序列安全降级，不能透传为 GUI 控制命令。

## 8. 替代 tmux 的会话与 PTY 模型

### 8.1 PTY 创建

`SpawnTerminal` 接收结构化 `ProcessSpec`：

```text
argv[] · cwd · env delta · rows/cols · pixels · TERM · locale · restart policy
```

`TERM` 与 locale 属于每次 Terminal 创建参数，而不是长期 worker 的全局环境。客户端发送 `TERM` 及 `LANG`/`LANGUAGE`/标准 `LC_*`；服务端按白名单验证，并确认目标 locale 已安装且为 UTF-8，否则回退到服务端可用的 UTF-8 locale。PTY 开启 `IUTF8`。任意环境变量透传和继承 gateway 全部环境都不允许。

禁止把未转义字符串拼成 `$SHELL -c`；只有用户明确请求 shell command 模式时才调用 shell。Unix MVP 使用 `forkpty/openpty + execve`，后续可评估 `portable-pty` 以支持 ConPTY。

### 8.2 布局归客户端

tmux 的 window/layout tree 不再决定终端如何显示：

- 创建窗格 = 客户端创建 UI pane，并可选择新建 Terminal 或附着已有 Terminal；
- 移动/缩放窗格通常只修改本地 layout；
- 用户选择“同步工作区布局”时，保存一个版本化 layout document 到服务端；
- 不同设备可有不同布局视图，同时观察同一组 Terminal；
- 不再存在必须共享的“当前 window”。

### 8.3 多客户端、输入租约和尺寸

同一 Terminal 可被多个客户端观察，但默认只有一个 `input lease owner`：

- lease 有短 TTL，并在活动时续租；
- 其他客户端只读；
- 接管需要显式操作，可通知或踢出旧 owner；
- 协作写入属于后续功能。

PTY 尺寸由 lease owner 控制。只读客户端按本地 viewport 做裁剪、缩放或 letterbox，不改变 PTY。这样避免 tmux 的“最小已连接客户端尺寸”问题。

### 8.4 滚动缓冲与历史

- 每 Terminal 默认内存保留 10,000 行或 8 MiB，取先达到者；
- 上限按用户配置，并设置服务端硬限制；
- 客户端按 `line_id` 分页请求历史；
- 默认不把完整终端内容永久写盘；
- 可选持久日志使用用户权限文件、分块压缩和静态加密；
- 审计默认只记录元数据，不记录按键和终端内容。

### 8.5 sessiond/worker 崩溃恢复

阶段 1：sessiond 单进程拥有 PTY，功能正确优先。

阶段 2：每 Terminal 独立 worker：

- worker daemonize 后持有 PTY master；
- 在 `$XDG_RUNTIME_DIR/qterm/<uid>/workers/<terminal-id>.sock` 接受 sessiond；
- 活动状态只来自 worker socket 握手，worker 另写最小 runtime manifest 用于发现；
- sessiond 重启扫描 socket/manifest，通过 Unix peer credentials 验证并重新注册；
- worker 崩溃只影响一个 Terminal；
- sessiond 可滚动升级，不关闭 PTY。

服务器重启后不承诺恢复进程；如果配置 systemd linger，sessiond 可在用户退出登录后继续运行。

### 8.6 清理策略

- RUNNING Terminal 默认无限期保留，管理员可设置 detached TTL；
- EXITED Terminal 保留最终屏幕和 exit status，例如 24 小时；
- 超过用户 PTY、内存或历史配额时拒绝创建新 Terminal，不擅自杀死活动会话；
- 提供 `qterm gc --dry-run` 和可恢复 tombstone；
- 所有显式 kill/delete 操作要求 request ID，返回明确作用对象。

## 9. 认证与部署模型

### 9.1 默认：Astra 原生连接与 SSH 式 TOFU

不使用 SSH bootstrap。Rootless 用户可以自行启动 daemon 并配置固定 UDP 端口；系统级部署由类似 sshd 的 gateway 监听共享端口。两种部署使用相同的 QUIC 握手和认证顺序：

1. 客户端直接与目标 UDP endpoint 建立 QUIC/TLS 1.3 连接；
2. 服务端证明其持有长期 TLS 主机私钥，客户端取得叶证书；
3. 客户端按 `host:port` 检查独立的 Astra known-hosts 文件；首次连接显示 SHA-256 指纹并要求确认，已记录证书发生变化时硬失败；
4. 主机身份确认完成后，客户端才发送目标用户名；
5. 服务端发送随机 challenge 和服务实例 ID，客户端使用 OpenSSH 格式私钥签署同时绑定 challenge、用户名和服务实例的认证 transcript；
6. 服务端用目标账户的 `~/.ssh/authorized_keys` 验证签名，并把连接路由到对应用户 worker；
7. 此后所有终端和其他通道复用同一条 QUIC 连接。

首次 TOFU 与 SSH 首次连接一样无法单独抵御当场的主动中间人，因此 UI 必须展示可经其他可信渠道核对的指纹。主机身份决定必须发生在用户名、用户公钥和签名发送之前。Astra 不向 `~/.ssh/known_hosts` 写入 X.509 数据。

### 9.2 可选：系统级共享 UDP/443

面向企业或多用户服务器：

- `qterm-gatewayd` 监听 UDP/443；
- 使用 ACME 证书或管理员提供证书；
- 默认使用目标账户的 OpenSSH `authorized_keys` 做公钥挑战，后续可增加 SSH-agent、mTLS、OIDC 或 PAM 插件；
- 验证用户后通过最小特权 helper 切换 UID/启动 sessiond；
- gateway 与 sessiond 通过用户隔离 Unix socket 通信；
- root monitor 不解析终端状态和文件内容；
- 所有连接、PTY、内存、Stream 和认证尝试有全局及用户级配额。

系统级模式必须单独安全审计，不能阻塞 rootless MVP。

### 9.3 主机证书和用户密钥

- rootless 和 managed 模式都默认使用 Astra known-hosts TOFU，不依赖 SSH bootstrap 或公开 PKI；
- managed 模式可由管理员预置/pin 证书，也可支持公开 CA 证书；
- daemon 长期保存 TLS 主机私钥，作用等同于 sshd 的 host key；客户端只保存证书 SHA-256 pin，不复制服务端证书文件；
- 主机证书轮换必须提供显式更新流程，不能在证书变化时静默覆盖旧 pin；
- 用户认证继续使用 OpenSSH 私钥格式和 `authorized_keys`，后续增加 ssh-agent、硬件密钥、加密私钥及更多算法；
- 不建议跨设备复制同一用户私钥；成熟版本应支持每设备独立密钥和单独撤销；
- QUIC session ticket 只用于加速握手，不等同于长期用户授权；ticket 和设备撤销记录必须有明确生命周期。

## 10. 文件与端口通道

虽然首个终端 MVP 可暂不实现完整 SFTP GUI，协议应从第一天预留独立通道，避免以后把文件塞进终端转义序列。

### 10.1 文件 API

基础操作：

- `stat/lstat/list/readlink`；
- 分页目录列表；
- `open/read/write/fsync/close`；
- 原子临时文件 + rename 保存；
- mkdir/remove/rename/chmod；
- offset + hash 的断点续传；
- 可选 checksum 和并行块。

每次大文件传输使用独立低优先级 QUIC Stream；元数据走 Control。服务端按当前 OS 用户权限操作，不默认获得额外权限。

安全要求：

- 长度、路径和符号链接有严格验证；
- 可选根目录限制使用 fd-relative/openat2 风格实现，不能只做字符串前缀检查；
- 上传默认写临时文件并原子替换；
- 解压、预览和媒体解析不在高权限 server 内完成。

### 10.2 端口转发

- 每个 TCP 连接映射一个 QUIC bidi Stream；
- UDP forwarding 后续使用 QUIC DATAGRAM + flow ID；
- 转发规则通过 Control 创建，有明确监听地址和授权；
- 默认只监听 loopback；
- 文件和转发均受带宽整形，不得压制终端交互。

## 11. 技术栈与仓库结构

### 11.1 推荐技术栈

| 层 | 选择 | 理由 |
|---|---|---|
| 语言 | Rust stable | 内存安全、跨平台、Quinn/async 生态、适合 daemon 与客户端 core |
| Runtime | Tokio | Quinn 原生支持，生态成熟 |
| QUIC | Quinn + rustls | 多流、DATAGRAM、rebind、迁移、优先级、纯 Rust |
| Control schema | Prost/Protobuf | 跨语言、可演进、Mosh 已验证此方向 |
| 压缩 | zstd | 快照和历史块，设置严格大小上限 |
| 活动注册表 | worker 内存 + Unix socket | PTY 持有者是唯一真相，避免持久状态与运行状态分叉 |
| 终端引擎 | `wezterm-term` 首选，接口隔离 | MIT、成熟的 cell/scrollback/现代转义支持 |
| PTY | Unix 原生封装；评估 `portable-pty` | 先保证 Unix 正确性，再扩展 ConPTY |
| 桌面 UI | Tauri 2 + xterm.js 作为首个参考客户端 | 快速跨平台；client-core 保持 UI 无关 |
| 可观测性 | tracing + metrics + qlog | 结构化诊断且不默认记录内容 |

终端引擎必须放在内部 trait 后：

```rust
trait TerminalEngine {
    fn feed_pty_output(&mut self, bytes: &[u8]) -> Vec<HostReply>;
    fn resize(&mut self, size: TerminalSize);
    fn generation(&self) -> u64;
    fn snapshot(&self, range: ScrollbackRange) -> Snapshot;
    fn diff_since(&self, generation: u64) -> DiffResult;
}
```

在 Phase 0 对 `wezterm-term`、`alacritty_terminal` 和 `libghostty-vt` 做独立评估，关注：服务端无 GUI 使用、序列化、滚动缓冲、查询回复、Unicode、双向文本、图片协议、API 稳定性和许可证。若首选不满足，替换不应影响网络协议。

### 11.2 建议仓库

```text
qterm/
├── Cargo.toml
├── crates/
│   ├── qterm-proto/          # protobuf schema、framing、版本和 ID
│   ├── qterm-transport/      # Transport trait、Quinn、SSH fallback
│   ├── qterm-termstate/      # 终端模型适配、snapshot/diff
│   ├── qterm-pty/            # PTY worker、进程、signal、resize
│   ├── qterm-session/        # workspace/terminal/attachment/lease
│   ├── qterm-auth/           # known-hosts、SSH key challenge、device identity
│   ├── qterm-files/          # 文件 RPC
│   ├── qterm-client-core/    # replica、reconnect、prediction、cache
│   └── qterm-testkit/        # netem、fake clock、PTY fixtures
├── bins/
│   ├── qterm/                # CLI client
│   ├── qterm-sessiond/
│   ├── qterm-worker/
│   ├── qterm-gatewayd/
│   └── qterm-desktop/
├── proto/
├── fuzz/
├── integration-tests/
└── docs/
    ├── protocol.md
    ├── threat-model.md
    ├── deployment.md
    └── compatibility.md
```

### 11.3 许可证

建议项目自身采用 Apache-2.0/MIT 双许可证：

- Quinn 为 MIT/Apache-2.0；
- tmux 为 ISC，可借鉴和在必要时复用小段代码并保留版权；
- WezTerm 为 MIT；
- Mosh 为 GPLv3，不能在非 GPL 项目中直接复制其实现；
- Mosh 的论文、协议思想和黑盒行为可用于独立实现，但发布前仍应做一次许可证审查。

## 12. 安全模型

### 12.1 威胁

- 未认证公网攻击者发送伪造 QUIC Initial/DATAGRAM；
- 中间人、DNS 污染和证书替换；
- 0-RTT 重放；
- 恶意本地用户连接其他用户的 worker socket；
- 恶意客户端耗尽 Stream、PTY、内存、历史或文件句柄；
- 终端输出携带恶意 OSC、图片、超长 Unicode 序列或压缩炸弹；
- 文件路径穿越、符号链接竞态和覆盖；
- compromised gateway 跨用户访问；
- 客户端设备丢失后的长期 token/密钥滥用。

### 12.2 控制措施

1. 使用 Quinn/rustls；默认 `ask`，生产自动化使用 `yes` 或 `accept-new`。兼容 SSH 的 `no` 模式必须醒目警告，不能成为部署默认值。
2. 公网服务启用 QUIC Retry、握手速率限制和连接配额。
3. known-hosts 按 `host:port` 绑定证书 SHA-256 pin；证书变化在发送用户名和用户签名前失败，文件采用安全权限、拒绝符号链接并原子更新。
4. 所有 mutation 等待 1-RTT；请求 ID 做幂等与重放检测。
5. 每用户 runtime 目录 `0700`、socket `0600`，并验证 Unix peer credentials。
6. gateway privilege separation；网络 worker 不长期保留 root。
7. 终端引擎、图片、压缩、Protobuf 和文件协议全部 fuzz。
8. 设置最大快照、最大行宽、最大 grapheme、最大 OSC/图片和最大解压比例。
9. OSC 52、通知、打开 URL、文件下载都进入客户端权限策略。
10. PTY 创建使用 argv 数组和环境 allow/deny policy，不拼接 shell 字符串。
11. 文件 API 使用 fd-relative 操作，防止 TOCTOU 和路径逃逸。
12. 日志默认不记录按键、密码、终端输出、文件内容、用户私钥或认证签名。
13. 正式版前完成独立安全审计和模糊测试覆盖审查。

## 13. 测试与验证计划

### 13.1 状态同步性质测试

核心性质：

```text
apply(snapshot(S)) == S
apply(diff(A, B), A) == B
drop(any intermediate patches); apply(next cumulative patch) == latest state
duplicate/reorder(datagrams) never regresses generation
```

使用 property-based testing 随机生成：字符、宽字符、组合字符、resize、滚动、alternate screen、颜色、删除/插入行和控制序列。

### 13.2 终端兼容测试

- vttest；
- Mosh 现有 emulation/prediction 行为作为黑盒对照，不复制 GPL 测试代码；
- tmux 的 ISC regress 用例中与 input、grid、reflow、OSC、Unicode、control backpressure 有关的部分；
- vim、nvim、emacs、less、htop、fzf、ssh、Claude Code/Codex 等真实 TUI；
- bash/zsh/fish 行编辑、bracketed paste、IME、OSC 7/8/52/133；
- 80/132 列边界、双宽字符、emoji ZWJ、双向文本。

### 13.3 网络测试矩阵

使用 Quinn simulated I/O、Linux `tc netem` 和真实 Wi-Fi/蜂窝切换：

| 条件 | 目标 |
|---|---|
| 0–20% 随机丢包 | 屏幕最终收敛，无输入丢失 |
| 100–1000 ms RTT | 输入可用，预测按策略开启 |
| 重排、重复、突发丢包 | generation 不回退，不显示损坏快照 |
| MTU 1280→较小路径 | 超大 DATAGRAM 自动转可靠 Stream |
| Wi-Fi→5G/VPN toggle | QUIC migration 或自动 reattach |
| UDP 完全阻断 | 自动提示并切换 SSH stdio fallback |
| 一个 pane 持续 `cat` 大文件 | 其他 pane 输入延迟受控 |
| 并行文件传输 | terminal/control 优先级有效 |

### 13.4 生命周期与混沌测试

- kill 客户端、sessiond、单个 worker；
- suspend/resume、设备休眠超过 QUIC timeout；
- sessiond 升级并重新接管 worker；
- worker 输出时客户端离线，随后请求历史；
- 同一 Terminal 两客户端争抢尺寸和输入租约；
- runtime socket/manifest 遗留、worker 握手失败；
- gateway draining 与版本滚动升级。

### 13.5 安全测试

- cargo-fuzz：Protobuf framing、DATAGRAM、snapshot、终端 parser、文件路径；
- 模糊压缩输入和超大长度；
- 0-RTT mutation 重放；
- token 跨连接、跨用户和过期复用；
- QUIC amplification/connection flood；
- symlink race、路径穿越、特殊设备文件；
- OSC 52/URL/图片资源滥用；
- 多租户 socket 和 UID 隔离。

### 13.6 性能与验收指标

初始指标，应在 Phase 0 基准后冻结：

- 同区域首次连接：不含人工认证时 `< 500 ms`；恢复连接 `< 1 s`；
- 网络迁移后首个可交互状态：新路径验证后 `< 2 RTT + 100 ms`；
- 服务端接收按键到写入 PTY：p99 `< 5 ms`（不含网络）；
- 状态变更到客户端渲染提交：p99 `< 16 ms`（不含网络）；
- 空闲 Terminal 基础内存目标 `< 4 MiB`，另加配置的滚动缓冲；
- 单连接 32 个活动 Terminal，任一后台高输出不使焦点输入 p99 增加超过 20 ms；
- 10% 丢包下，客户端最终状态与服务端一致；
- 连续 24 小时网络抖动测试无 PTY 泄漏、无无界队列。

## 14. 可观测性与运维

- 结构化 tracing：connection/terminal/attachment/request ID；
- Prometheus 可选指标：连接数、PTY 数、内存、状态帧、快照、丢包、RTT、迁移、重同步、fallback；
- qlog 默认关闭，用户显式开启并自动脱敏 endpoint/token；
- 每 Terminal 暴露状态：running/exited、PID、cwd、title、last activity、attachments、buffer usage；
- `qterm doctor` 检查 UDP、主机证书/known-hosts、用户密钥、runtime 权限、TERM/locale 和 daemon 版本；
- gateway 支持 drain，新连接拒绝，现有 Terminal 不受影响；
- 协议和存储 schema 分开版本，支持 N/N-1 客户端兼容。

## 15. 兼容与迁移策略

### 15.1 SSH 生态

- 解析并尊重 `~/.ssh/config` 的 Host、User、Port、IdentityFile、ProxyJump；
- QUIC 主连接不调用系统 OpenSSH；SSH 配置只作为目标、用户、密钥和可选 fallback 的配置来源；
- UDP 不通时继续走同一 SSH 通道；
- 支持 SSH agent，不默认复制私钥进应用 vault。

### 15.2 tmux 迁移

无法无损把已经运行的任意进程从 tmux PTY 移到新 PTY。迁移策略：

1. 新工作默认创建 QTerm Terminal；
2. 旧 tmux session 继续运行直至自然结束；
3. 可选实现 `tmux -CC` bridge，把既有 tmux pane 暂时暴露给 QTerm 客户端；
4. bridge 只作为过渡层，不成为新架构依赖；
5. 提供常用配置映射：default shell、history limit、startup command、environment。

### 15.3 Mosh 迁移

- 不尝试 wire compatibility；
- `qterm connect` 提供与 `mosh host` 相似的命令体验；
- UDP 不可用时比 Mosh 多一个 SSH fallback；
- Mosh 可继续并存，用户可逐主机切换。

### 15.4 终端兼容

- 初期 `TERM=qterm-256color`，同时提供 terminfo；
- 在 terminfo 未安装时退化到 `xterm-256color`；
- CLI client 能把语义状态转换为 ANSI，从任意外层终端使用；
- GUI client 使用相同 client-core，不定义第二套协议。

## 16. 实施阶段、人员与退出标准

以下按 5 人团队估算：协议/网络、PTY/终端、服务端安全、客户端、测试/可靠性。阶段可部分并行。

### Phase 0：高风险技术验证（4–6 周）

交付：

- Quinn 单连接、多 Stream、DATAGRAM、priority 和 `Endpoint::rebind` spike；
- QUIC 主机证书 TOFU、known-hosts 和 OpenSSH 用户密钥 challenge-response spike；
- 一个 PTY worker 在客户端断开后继续运行；
- 三个终端引擎的解析、snapshot、diff 和内存基准；
- 可靠 raw stream 与累计 semantic patch 的对比；
- 初版 threat model 与 wire schema。

退出标准：

- Wi-Fi/接口切换后连接或 reattach 成功；
- 10% 丢包下一个终端最终状态收敛；
- 证明选定 terminal engine 能无 GUI 运行并生成完整快照；
- 明确 rootless UDP endpoint 策略。

### Phase 1：可用的核心 MVP（8–10 周）

交付：

- qterm-sessiond、内嵌 worker、CLI client；
- 一连接多 Terminal；
- 可靠 Stream 输出、输入、resize、signal、exit status；
- detach/attach 和可靠完整快照；
- workspace、input lease、仅由活动 worker 构成的运行时注册表；
- SSH stdio fallback；
- 基础配额、日志和 `qterm doctor`。

退出标准：

- 不安装 tmux 可同时运行 32 个远端 Shell；
- 杀死/重启客户端后全部恢复；
- UDP/TCP fallback 自动化测试通过；
- 真实 shell/TUI smoke matrix 通过。

### Phase 2：Mosh 式状态同步（8–10 周）

交付：

- generation、row version、scrollback line ID；
- QUIC DATAGRAM 累计补丁与 ACK；
- snapshot fallback 和 patch coalescing；
- per-pane 优先级、慢客户端背压；
- 网络模拟、性质测试和 24 小时 soak test；
- worker 独立进程与 sessiond 重接管。

退出标准：

- 丢任意中间状态包仍收敛；
- 大输出 pane 不明显影响焦点 pane；
- sessiond 被 kill/restart 时 worker 和 PTY 存活；
- 无无界队列或历史增长。

### Phase 3：桌面客户端与文件通道（8–10 周）

交付：

- Windows/macOS/Linux 桌面参考客户端；
- 原生应用标签、窗格、工作区、布局保存；
- 滚动缓冲分页、搜索、复制；
- 文件浏览、上传、下载、编辑和断点续传；
- SSH config/agent 集成；
- 基础预测 overlay。

退出标准：

- 日常使用无需 tmux/Mosh/SFTP 外部客户端；
- 桌面三平台安装包和自动更新；
- 文件安全测试与路径穿越测试通过。

### Phase 4：Managed gateway 与生产加固（10–14 周）

交付：

- UDP/443 多用户 gateway、privilege separation；
- device identity、撤销、mTLS/OIDC/SSH-key auth；
- 审计、全局配额、draining、滚动升级；
- 外部安全审计、模糊测试和负载测试；
- 协议 v1 冻结。

退出标准：

- 安全审计高危问题清零；
- 多租户隔离测试通过；
- N/N-1 客户端兼容；
- 生产部署、备份、升级和故障恢复文档完成。

### Phase 5：移动端与协作（可选，8–12 周）

- iOS/Android；
- 移动网络 rebind 与后台恢复；
- 只读分享、输入接管和多设备布局；
- 硬件密钥与平台安全存储。

## 17. 主要风险与缓解

| 风险 | 等级 | 缓解 |
|---|---:|---|
| 终端仿真边角远超预期 | 极高 | 复用成熟引擎、接口隔离、vttest/真实 TUI/模糊测试 |
| 语义状态与客户端渲染不一致 | 高 | 服务端权威、generation、原子快照、性质测试 |
| QUIC 被企业网络封锁 | 高 | SSH stdio fallback 从 v1 就存在 |
| 单连接 bulk 流量饿死交互 | 高 | Quinn priority、公平调度、文件限速、焦点帧优先 |
| 多客户端尺寸冲突 | 高 | 单 input/resize lease，观察者不改变 PTY |
| Managed daemon 扩大攻击面 | 极高 | rootless 优先、privilege separation、独立审计 |
| sessiond 崩溃关闭所有 PTY | 高 | 每 Terminal worker、Unix socket 重注册 |
| 0-RTT 重放产生副作用 | 高 | mutation 全部等待 1-RTT、请求幂等键 |
| 移动系统杀进程 | 中 | 应用级 reattach，不把 QUIC migration 当持久化 |
| Mosh GPL 污染许可证 | 中 | clean-room 行为实现、禁止复制、发布前法律审查 |
| 功能范围膨胀 | 高 | 先 CLI/核心，再 GUI/文件，再 managed/mobile |

## 18. 必须尽早冻结的决策

建议现在接受以下决策：

1. Rust/Quinn 作为首个实现，不手写 QUIC。
2. 默认直接 QUIC + Astra known-hosts TOFU + OpenSSH 用户密钥认证；不使用 SSH bootstrap。
3. 服务端持有权威终端状态；客户端不只接收原始字节。
4. raw reliable mode 先完成，DATAGRAM 状态同步在其后，不反过来阻塞 MVP。
5. 布局归客户端，服务端管理 Terminal 而不是文本窗口管理器。
6. 单 Terminal 单输入/resize lease；协作多写后置。
7. QUIC primary + SSH stdio fallback 共用一套协议。
8. 不承诺主机 reboot 后恢复任意进程。
9. 项目采用 Apache-2.0/MIT，Mosh 仅作行为参考。
10. 文件和转发是独立通道，绝不通过终端转义序列实现。

仍需产品方确认：

- 首发是个人 rootless 工具，还是必须从第一天支持企业多用户；
- v1 是否必须包含桌面 GUI，还是 CLI/SDK 可先发布；
- 是否必须首发移动端；
- 是否需要团队共享/审计；
- 可接受的默认 detached TTL 和服务端资源上限。

## 19. v1 Definition of Done

满足以下条件才称为 v1，而不是原型：

- 一个 host/user QUIC 连接中稳定运行 32 个 Terminal；
- 所有 Terminal 都由服务端 PTY broker 持有，不依赖 tmux；
- Wi-Fi/蜂窝/VPN 变化可迁移或自动 reattach；
- 客户端 kill/restart 后恢复屏幕、滚动缓冲、cwd/title 元数据和 exit status；
- 中间状态 DATAGRAM 丢失后最终状态可证明收敛；
- 输入可靠且不会因输出丢包重复执行；
- 一个高输出 pane 和大文件传输不会饿死其他交互通道；
- UDP 被阻断时 SSH fallback 可用；
- rootless 模式无需管理员安装 daemon；
- 支持三大桌面客户端平台；
- 威胁模型、协议、部署、升级和故障排查文档完整；
- fuzz、network chaos、24 小时 soak 和外部安全审计通过。

## 20. 最终建议

最稳妥的工程顺序是：

```text
先替代 tmux 的 PTY 持久层
        ↓
再用 QUIC 实现一连接多通道和迁移
        ↓
先以可靠 Stream 得到正确系统
        ↓
再加入 Mosh 式累计状态 DATAGRAM
        ↓
最后实现预测、GUI 文件管理和多用户 gateway
```

原因是：PTY 生命周期和终端状态是产品正确性的根；QUIC 已经解决通用网络问题；Mosh 式不可靠状态同步是性能优化，不能成为第一阶段正确性的前提。若反过来先做 UDP 屏幕差分，很容易得到一个网络演示，却没有可靠的会话、权限、恢复和资源模型。

这个项目真正的新价值不是“QUIC 版 Mosh”或“有 GUI 的 tmux”，而是一个清晰分层的远程终端操作系统：QUIC 是连接层，Terminal State Replication 是交互层，sessiond/worker 是持久进程层，客户端布局是表现层。四层可以独立测试、替换和演进。

## 21. 主要参考

- [RFC 9000 — QUIC Transport](https://www.rfc-editor.org/rfc/rfc9000.html)
- [RFC 9001 — Using TLS to Secure QUIC](https://www.rfc-editor.org/rfc/rfc9001.html)
- [RFC 9002 — QUIC Loss Detection and Congestion Control](https://www.rfc-editor.org/rfc/rfc9002.html)
- [RFC 9221 — QUIC DATAGRAM](https://www.rfc-editor.org/rfc/rfc9221.html)
- [Mosh source](https://github.com/mobile-shell/mosh)
- [tmux source](https://github.com/tmux/tmux)
- [Quinn source](https://github.com/quinn-rs/quinn)
- [tmux Control Mode](https://github.com/tmux/tmux/wiki/Control-Mode)
- [WezTerm Multiplexing](https://wezterm.org/multiplexing.html)
- [PtyMux protocol discussion](https://github.com/wezterm/wezterm/discussions/4889)
