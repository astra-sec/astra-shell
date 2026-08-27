# Astra server TerminalEngine

状态：`TERM-03` 至 `TERM-06`、`HIST-01/02` 服务端实现说明。服务端权威终端模型、可靠 semantic attach、输入模式、宿主能力边界、主屏历史 cell model 和 Anchor 分页已经实现；增量同步和历史容量策略仍属于后续任务。

## 权威数据路径

```text
PTY reader
  -> TerminalEngine::advance(bytes)
     -> astra-wezterm-term (唯一 VT parser/grid/history)
        -> AstraTerminalView (只读、有界视图)
           -> Terminal State v2 (直接语义导出)
```

每段 PTY 输出在发布 `PtyEvent::Output` 以前先进入 `TerminalEngine`，resize 也先更新权威 grid 再调用 PTY resize。`Terminal::semantic_state_and_subscribe` 在同一 engine mutex 内先建立 event subscription 再导出 State，因此 snapshot 与后续事件之间有原子边界。

服务端不再依赖 `vt100`，也没有第二套 cell model。程序 title、主屏历史、备用屏、cursor/saved cursor、modes、margins、tabs、palette、style、hyperlink 和 cwd 都来自同一个引擎实例。

## 受控 fork 与供应链

- fork：`vendor/astra-wezterm-term`
- 上游：WezTerm commit `78cd82dbba7315814bfbff40e246b8bed4b702e7`
- 默认 feature：`astra-headless`
- Astra v1 未承诺的 graphics/image 路径默认不编译，也不在 DA 中声明 Sixel。
- 未独立发布的 `wezterm-cell`、`wezterm-char-props`、`wezterm-escape-parser`、`wezterm-surface` 是同一上游 commit 的源码镜像，放在 `vendor/`；Astra 修改只允许进入 `astra-wezterm-term`。
- fork 不包含 protobuf、网络 generation 或客户端特判；升级必须固定新 commit、审计 upstream diff、同步替换四个 core mirror 并重跑 conformance suite。

`AstraTerminalView` 同时暴露 primary/alternate 和必要运行态，只提供 caller-bounded row iterator，不提供无界 clone-all API。网络层将该 view 转换为 Astra 自有、带资源上限的 Terminal State v2，不序列化上游私有结构。

## 主屏历史模型

历史不是另一份字符串缓存。主屏 viewport 上滚时，原 `Line` 连同 grapheme、cell width、完整 attributes、hyperlink、soft-wrap marker 和 `AstraLineIdentity` 一起进入同一个 `VecDeque`；压缩 scrollback 只改变内部存储，不降低导出语义。`AstraScreenView` 明确暴露 screen kind、是否允许 scrollback 和 history row count，`TerminalEngine` 每次导出都验证：

- primary 的 retained history 与 viewport 连续，`viewport_start + rows == row_count`；
- alternate 不允许 scrollback，history row count 和 viewport start 必须为 0，row count 必须正好等于 viewport 高度；
- primary/alternate 仍共享 Terminal State v2 的有界消息预算，但只有 primary 能产生历史。

`HIST-01` 只定义权威 cell model 和稳定 identity。它不提高 2,000 行临时容量基线，也不让客户端自行拼接页面；容量/字节配额与滚动条仍分别属于 `HIST-03/04`。

## Anchor 历史分页

选择 `terminal.history_paging` v1 的 semantic attachment 可在同一可靠双向 stream 上发送 `HistoryPageRequest`。请求必须携带当前 16-byte epoch、完整 `before: Anchor` 和 `1...512` 行上限；服务端只从 primary retained history 中返回严格位于 `before` 之前、最多 512 行的连续页面，不读取 alternate，也不建立第二份历史缓存。

`HistoryPage` 携带请求 sequence、epoch/generation、cols、真实 oldest/newest、页面首尾 Anchor、页面实际引用的 style/hyperlink 表和 `more_before`。页面编码上限 4 MiB，以 512 KiB chunk、整体 SHA-256 和独立 transfer ID 可靠传输。epoch 已变化时返回无 rows 的 `reset_required` 页面，客户端必须丢弃旧页；请求边界已被 trim 时返回无 rows 的正常页。分页不会改变 engine、generation 或 scrollback 容量。

## 逻辑行身份

每个 `Screen` 用与行队列严格 lockstep 的 sidecar 保存：

```text
AstraLineIdentity {
  logical_line_id: u64,
  cell_offset: usize
}
```

不变量：

- primary ID 从 1 分配，alternate ID 从 `1 << 63` 分配，永不复用。
- soft-wrap 的所有物理分段共享 logical ID，`cell_offset` 按当前 terminal columns 递增。
- resize/reflow 保留 logical ID，并重新计算物理分段及 offset。
- trim 只移除对应 sidecar entry；append、scroll 和 resize 始终同时变更 line 与 identity。
- 在 retained rows 上方插入等无法保持 ID 排序的结构编辑会重建当前 identity space，并递增 fork identity epoch；`TerminalEngine` 随即更换 wire epoch，使旧 anchor 原子失效。

wire generation 和 row/cursor version 相对于当前 epoch 计算。新 epoch 的 generation 从 1 开始；epoch 前已存在的行 version 归一为 1，后续变化使用 engine sequence 相对值。WezTerm `StableRowIndex` 不进入协议。

## Host replies 与安全边界

WezTerm 的 DA/DSR 等 PTY reply 通过宿主提供的 writer 回写同一个 PTY。writer 与用户输入共用 mutex，避免字节交错；reply 不经过客户端 round trip。

DA/DSR 和尺寸查询由服务端直接回答。字符尺寸始终可用；像素尺寸由客户端随 `Resize` 提交，两个像素字段必须同时为零或同时有效。像素未知时，服务端不伪造 `0x0` 响应，而是对像素查询安全地不响应。title reporting、checksum、graphics、notification 和 download 默认关闭且不在 DA 中宣称。

OSC 52 是单向、显式协商的 host effect：服务端最多接受 256 KiB UTF-8 写入，通过 `terminal.clipboard_write` v1 结构化事件交给 semantic client；query 永不读取或回传客户端剪贴板，未协商的 attachment 不收到事件。Primary DA 不宣称通用 clipboard access。OSC 8 hyperlink 只作为语义 State 数据传输，是否打开以及允许的 URL scheme 必须由客户端在用户点击时决定。

每次语义导出都运行 Terminal State v2 validator；超出 schema 大小、引用或 enum 约束时返回错误并拒绝整个 State，不部分发送，也不 panic。历史总容量、按字节配额和分页不是本任务职责，仍由 `HIST-01` 至 `HIST-03` 与 `OPS-01` 完成。

## 迁移兼容边界

旧 Apple/Rust CLI 只协商 `terminal.legacy_ansi_snapshot`。为保持 N-1 客户端可连接，`TerminalEngine::legacy_snapshot` 将已经验证的语义 State 单向渲染为旧 `TerminalSnapshot`；它不是权威状态，也不回灌引擎。这是隔离的 N-1 兼容路径，不能继续扩展字段或行为，并将在 application v4 最早删除。

选择 `terminal.semantic_state` v2 的 attachment 不发送 raw PTY 或 ANSI snapshot，而是发送带整体 SHA-256 的可靠 `TerminalStateChunk`。Apple 客户端验证并原子发布完整 State，通过受控 SwiftTerm fork 的 cell import API 同时替换 primary/alternate buffer；未选择 capability 的客户端才进入上述 N-1 路径。当前每次输出发送完整 State，后续 `SYNC-01/02` 将加入 ACK 和累计 diff，不能改变这条单权威链路。

## Conformance evidence

根 crate 测试覆盖：

- primary/alternate 同时导出；wide grapheme、style、hyperlink 和 modes 保留。
- resize/reflow 后 logical line ID 与 wire epoch 保持。
- retained rows 上方结构插入会轮换 epoch，并从 generation 1 重新开始。
- primary history 与 alternate viewport 共享 schema 的 4096-row 总预算；截取后仍完整包含当前 primary viewport。
- 带 style、hyperlink 和 wide grapheme 的 soft-wrapped logical line 滚入历史后保持完整 cell 语义。
- normal scroll/trim 不轮换 epoch、不复用 logical ID；反复 narrow/wide reflow 保持 logical ID 和 retained content。
- alternate 大量滚动仍只导出一个 viewport，且不改变 primary 历史的 cell 和 identity。
- Anchor 分页严格位于请求边界之前，遵守 512-row/4-MiB 上限；oldest 边界产生空页，旧 epoch 产生结构化 reset。
- 历史页分片共享 transfer identity 和 SHA-256；N-1 decoder 忽略未协商的 command/event oneof。
- DA/DSR、字符/像素尺寸查询写入宿主 sink；未知像素、title 和 OSC 52 read 安全降级。
- OSC 52 write 形成有大小上限的结构化 host effect，超限写入被拒绝。
- RIS 后旧内容不再进入后续语义 State；ANSI 兼容视图从语义 State 生成。

独立 fork package 也必须能在默认 `astra-headless` feature 下完成 `cargo test`。完整 server suite 是最终集成门禁。
