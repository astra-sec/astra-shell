# ADR 0005: 每用户一个长期 worker

- 状态：已接受
- 日期：2026-08-27
- 任务：WORK-01、WORK-02、WORK-03
- 取代：原计划中的“每 Terminal 一个 worker”进程边界

## 背景

managed 模式当前由 root gateway 为每个 Unix UID 启动一个降权 worker。该 worker
拥有这个用户的 `SessionManager`、Workspace catalog、Terminal/PTY、Attachment 和文件
操作。原计划准备继续拆成每 Terminal 一个 worker，以便单个进程崩溃不影响其他
Terminal，并让 sessiond 逐个重接管。

每 Terminal 一个进程会把同一用户的一条 QUIC 连接、Workspace CRUD、用户配额、文件
操作和 Terminal 路由拆到大量 socket、manifest 与进程之间。对于 Astra 当前的个人和
小团队服务器定位，这种复杂度高于单 Terminal 故障隔离带来的收益。Workspace 是服务端
资源容器，不是进程或安全主体；一个 Workspace 包含多个 Terminal，也不应隐式创建或
折叠 worker。

## 决策

### 进程与所有权边界

```text
root gateway
└── UID worker（长期、非特权、每用户最多一个）
    ├── SessionManager
    ├── Workspace A
    │   ├── Terminal 1 / PTY
    │   └── Terminal 2 / PTY
    ├── Workspace B
    │   └── Terminal 3 / PTY
    └── Attachment、Files 与用户级 ResourcePool
```

- managed 模式每个 UID 同时最多有一个活动 worker；所有 Workspace 和 Terminal 由该
  worker 统一管理。
- gateway 只负责认证、协议协商、全局/用户容量准入和字节流代理，不读取或复制
  Terminal 状态。
- Workspace CRUD 不创建、终止或迁移 worker。Terminal 在 Workspace 间的归属变化是
  `SessionManager` 内的领域操作，不是进程调度操作。
- rootless 模式继续由当前用户 daemon 直接持有同一组对象，不伪造额外 worker 层。
- 用户级 worker 是 privilege-separation 和资源所有权边界；Terminal 仍是协议、状态、
  lease、配额和可观测性的独立对象。

### 发现与重连

gateway 可以退出或升级，而不结束已存在的用户 worker。重新连接时，gateway 按 UID
定位 runtime 目录，验证目录/socket 权限、Unix peer credentials、worker identity、
内部协议版本和实例 epoch 后复用 worker。遗留 PID 文件本身不是存活或身份依据；发现
必须以成功的受认证 socket 握手为准。

同一 UID 的并发连接必须汇聚到同一个 worker。启动、发现和退出之间要有原子仲裁，不能
因竞争产生第二个可服务 worker，也不能让新的连接进入正在 draining 的实例。

### 故障域

- 接受 worker 崩溃会结束该用户当前全部 PTY/Terminal；故障不会跨 UID 扩散，也不能
  破坏其他用户 worker 或 root gateway。
- 不把“单个 Terminal 崩溃不影响同用户其他 Terminal”列为 v1 完成条件。终端引擎的
  panic containment、进程级 sandbox 或 PTY broker 可以在真实故障数据证明必要后另立
  ADR，不能恢复为隐含前置。
- worker 丢失后客户端必须得到明确的 session/epoch 失效结果并重新列举，而不是显示
  陈旧的 running Terminal 或尝试 ANSI replay。

### Draining 与升级

- gateway draining 拒绝新连接或把新连接交给新 gateway；已认证连接与用户 worker 的
  Terminal 不因 gateway 二进制替换而结束。
- 活跃用户 worker 不承诺跨二进制版本无损热替换，也不把 PTY reparent/checkpoint
  伪装成已实现能力。
- worker binary generation 必须可观测。升级时允许不同 UID 的旧、新 worker 暂时并存；
  没有 Terminal、Attachment 和文件操作的 worker 可退出并由新版本按需重建。
- 对仍有 Terminal 的旧 worker，默认等待自然清空；强制换代必须是显式管理操作，并在
  执行前报告会影响该用户全部 Terminal。
- 内部 gateway↔worker 协议必须支持明确的 N/N-1 窗口，使 gateway 先升级时仍能代理
  既有 worker。超过兼容窗口的 worker 不得静默复用。

## 后果

保留当前按 UID 的简单路由、配额 bundle 和 privilege separation，避免每 Terminal
进程带来的跨进程 Workspace 协调。代价是同一用户的 Terminal 共享进程故障域，活跃
worker 的安全修复不能自动获得“所有 PTY 无损迁移”语义。运维界面、doctor 和审计必须
按 worker generation 与受影响 UID 清楚呈现这个边界。

## 不在本 ADR 范围

- 主机重启后恢复 PTY 或 Terminal 内容；
- 跨 worker checkpoint、PTY reparent 或 CRIU；
- 每 Workspace 一个 worker；
- 将客户端 tab、pane 或 Files 布局同步到服务端；
- 用本地缓存或 ANSI replay 掩盖 worker 丢失。

## 验收

- 并发认证与 gateway 重启后，同一 UID 始终只复用一个经过 peer credential、identity、
  epoch 和内部协议校验的 worker。
- 一个 worker 可以同时管理多个 Workspace，每个 Workspace 可以包含多个 Terminal；
  CRUD 和切换不改变 worker 身份。
- gateway 重启、drain 和二进制升级不结束已存在的用户 worker/PTY；N/N-1 内部协议兼容
  有自动化测试。
- 空闲旧 generation worker 能安全退出并由新版本替代；活跃旧 worker 保持服务或通过
  显式破坏性操作终止，不发生隐式换代。
- kill 一个用户 worker 的混沌测试只影响该 UID，客户端收到 epoch/session 失效并清除
  陈旧运行态；其他 UID 的连接与 Terminal 保持可用。
- 资源测试继续证明同 UID 只有一个 worker bundle，Terminal/Attachment/File 配额在该
  bundle 内独立计量。
