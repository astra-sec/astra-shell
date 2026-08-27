# Astra Terminal State v2

状态：`TERM-02` 领域模型；`TERM-03` 从服务端权威引擎直接导出；`TERM-04` 已接入可靠 wire 分片、Apple replica 和 SwiftTerm 受控 fork 的原子 cell 导入。

权威 schema 是 [`proto/terminal_state_v2.proto`](../../proto/terminal_state_v2.proto)。旧 `TerminalSnapshot` 的 ANSI 字段不是 v2 的组成部分。

## 身份与顺序

- `epoch` 必须正好 16 bytes。服务端权威引擎重建、状态身份空间无法连续或 generation 溢出前必须生成新 epoch。
- `generation` 在一个 epoch 内从 1 开始严格递增。客户端不得应用更小或相等 generation 的替换状态。
- `logical_line_id` 在一个 epoch 内从 1 开始单调分配，永不复用。soft-wrap 和 resize/reflow 不改变它。
- 一个逻辑行可产生多个物理 `Row`。`Anchor(logical_line_id, cell_offset)` 是总排序键；相同逻辑行的 `cell_offset` 必须严格递增。
- `row_version` 是该物理行最后变化时的 state generation，范围为 `1...generation`。reflow 产生的新物理分段必须获得当前 generation。
- wire scroll anchor 必须使用完整 `Anchor`；WezTerm 的物理 `StableRowIndex` 不得进入协议。

## 屏幕语义

- `primary.included_rows` 是服务端当前保留范围内的一个连续、有界片段，可在 viewport 前携带历史。
- `primary.viewport_start` 指向 viewport 第一行；从该位置起必须至少有 `State.rows` 行。
- `alternate` 没有 scrollback：`viewport_start == 0` 且 `included_rows.count == State.rows`。
- primary 与 alternate 始终同时存在，`active_screen` 只决定当前显示哪一个，不能用于省略非活动屏。
- `oldest_available`/`newest_available` 描述服务端全量可用范围；`included_start`/`included_end` 描述本消息实际携带范围。首尾必须与 `included_rows` 一致。
- `Row.cells` 是按 column 严格递增的稀疏 grapheme。未出现的列是 style 0 的空白。grapheme width 只能是 1 或 2，且不得越过 `State.cols`。
- `wrapped_to_next` 只描述当前 cols 下的物理软换行；逻辑硬换行由下一行获得新的 `logical_line_id` 表示。
- cursor 同时携带 viewport `(x,y)` 和逻辑 `anchor`。两者必须指向同一位置；客户端用 `(x,y)` 渲染，用 anchor 在 reflow/历史合并后校验身份。`wrap_pending` 为 true 时 cursor 仍显示在最后一列，但下一个 printable grapheme 必须先按 DECAWM 换到下一行；不能把引擎内部可能等于 `cols` 的 pending-wrap x 截断后丢失该语义。
- scroll margin 的 top/left 为 inclusive，bottom/right 为 exclusive；默认全屏范围是 `0...rows` 与 `0...cols`。

## Style、颜色与 hyperlink

- style 0 是 schema 定义的默认 style，不出现在 `styles` 表中：palette foreground/background、normal intensity、无 underline/blink/italic/reverse/strike/invisible/overline、output semantic type、baseline vertical align。显式 style ID 从 1 开始且在一个 State 内唯一。
- hyperlink ID 0 表示无链接；显式 hyperlink ID 从 1 开始且唯一。
- cell 引用的 style/hyperlink 必须存在。接收方不得用“找不到就默认”掩盖损坏状态。
- `Color.default_color` 必须以 oneof presence 表示；`false` 和 `true` 的 payload 值都只表示 default 分支存在。
- `rgb` 和 palette 中的 RGB 均为 `0xRRGGBB`，高 8 bits 必须为零。
- 完整 State 必须携带 256 个 indexed palette entry，保证 palette-index cell 在 replica 上可直接渲染。

## 资源上限

接收方在使用状态前必须运行结构验证。v2 初始硬上限：

| 项目 | 上限 |
|---|---:|
| protobuf encoded State | 8 MiB |
| rows / cols | 各 1...1,000 |
| 两屏 `included_rows` 总数 | 4,096 |
| 所有 row 的 cell 总数 | 1,000,000 |
| 单 grapheme UTF-8 | 256 bytes |
| styles | 4,096 |
| hyperlinks | 4,096 |
| 单 hyperlink URI | 16 KiB |
| title | 512 bytes |
| working directory | 16 KiB |
| tab stops / screen | 1,000 |

8 MiB 是单个语义状态对象的领域上限，不表示它必须塞入一个现有 `astra/1` 4 MiB frame。`PROTO-01`/`HIST-02` 必须定义可靠分帧和历史分页；禁止为了通过 frame 限制而截断 cell、style 或另一屏。

验证失败是协议错误：丢弃整个 state，保留最后一个已验证 generation，并请求可靠 snapshot；不得部分导入。

## 版本演进

- 当前 `schema_version` 必须为 2。
- 同一 major schema 内只能追加字段、追加 enum value、或放宽接收端可忽略的提示信息；不得改变现有 tag 的语义或 wire type。
- 删除字段时必须先 `reserved` 其 tag 和名称，并经过 `PROTO-01` 定义的 N/N-1 迁移窗口。
- 新增影响渲染/输入正确性的必需语义时，必须增加 capability；旧接收端不能仅靠 protobuf unknown-field 行为假装支持。
- 改变 anchor、generation、cell、屏幕原子性或资源上限含义需要新的 schema major version。
- enum 的未知数值必须导致当前 v2 state 验证失败；能力协商后才能允许新值。

## 与后续任务的边界

- `TERM-03` 已从唯一 WezTerm fork 导出本 schema，并证明权威导出不经过 ANSI/纯文本；实现边界见 [`docs/architecture/terminal-engine.md`](../architecture/terminal-engine.md)。
- Apple 端先完成分片元数据、SHA-256、protobuf 和本 schema 的完整验证，再由单一 `AstraTerminalReplica` 按 epoch/generation 发布；SwiftTerm parser 不参与 semantic attachment。
- 当前可靠路径在 PTY 更新和 lag 恢复时发送完整 State。它是 `SYNC-01/02` 之前正确但带宽较高的基线；后续 diff/ACK 只能替换传输增量，不能建立第二个 replica。
- `HIST-02` 复用 `Anchor` 定义分页请求、页范围和 merge，不重新定义字节/行偏移身份。
- `SYNC-01` 在本 schema 外层定义 base/target generation、ACK 和 diff；不得复用 `row_version` 充当 transport ACK。
