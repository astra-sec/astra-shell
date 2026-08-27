# ADR-0001：服务端权威终端引擎选择

- 状态：Accepted
- 日期：2026-08-26
- 对应任务：`TERM-01` / 原计划 `P0-04`
- 决策范围：服务端权威 VT 模型；不决定 wire schema 和 Apple renderer

## 背景

Astra 当前让服务端 `vt100` 解析 PTY 输出，再把画面编码成 ANSI，Apple 客户端又交给 SwiftTerm 解析。ANSI 是操作流，不是状态序列化；这条路径无法无损表达主屏历史、备用屏、cell 属性、软换行、模式和稳定滚动锚点。

目标架构是：

```text
PTY -> 唯一权威 TerminalEngine -> Astra Terminal State v2 -> Client Replica -> Renderer
```

引擎必须 headless，能保留完整 cell/history，处理 VT 模式与终端查询回复，支持 resize/reflow，并允许 Astra 导出自己的有界语义 snapshot/diff。上游引擎的私有内存布局或 serde 格式不能成为网络协议。

## 决策

服务端采用 **固定提交上的 Astra 最小 fork of `wezterm-term`**：

- 上游固定基线：`78cd82dbba7315814bfbff40e246b8bed4b702e7`。
- 许可证：MIT。
- Astra 通过受控 fork/patch 集成，不从上游 `main` 浮动拉取，也不使用第三方重新打包 crate。
- Astra Terminal State v2 是独立、版本化、有大小上限的协议；不直接序列化 `TerminalState`、`Screen`、`Line` 或其 Rust 内存结构。
- `vt100` 在 `TERM-03` 完成迁移后移除；迁移期间不得扩展其 ANSI snapshot 能力。

这个选择的核心理由不是 ANSI 覆盖数量，而是 WezTerm 已有的三项结构基础组合最好：压缩行存储、逐行 `SequenceNo`、以及 scrollback trim 后仍单调的 `StableRowIndex`。它们能支撑语义状态复制，同时实测内存显著低于 Alacritty 候选。

`StableRowIndex` 只在当前物理行分段内稳定，resize/reflow 会合并并重新切分物理行，因此 **不能直接作为 Astra wire `line_id`**。Astra fork 必须增加独立的逻辑行身份，协议滚动锚点使用逻辑行 ID 加逻辑 cell offset；物理 wrapped segment 由当前列宽派生。

## 候选评估

评估固定在同一天的三个上游提交：

- WezTerm：`78cd82dbba7315814bfbff40e246b8bed4b702e7`
- Alacritty：`ede2ac144da4dec4c075bfa803aacf3b3739bce6`
- Ghostty：`5f5b988c5236facfe8d2439203d9ee9d5b636cf8`

| 维度 | `wezterm-term` | `alacritty_terminal` | `libghostty-vt` |
|---|---|---|---|
| Headless | 原生 `Terminal`，宿主提供 writer，实测编译和运行通过 | 原生 `Term` + `vte::ansi::Processor`，实测编译和运行通过 | C headless API 设计完整；本机无 Zig，未执行二进制实验 |
| Cell/grapheme/style | `Cell` + `CellAttributes`，支持 grapheme、宽度、颜色、hyperlink、wrap 等 | `Cell` 支持字符、零宽字符、宽字符、颜色、flags、hyperlink | C API 暴露 screen/cell/grid ref，覆盖完整 VT 模型 |
| 主屏历史 | `Screen` 使用 `VecDeque<Line>`，支持压缩行、scrollback、逐行 seqno 和 stable/physical 映射 | `Grid<Cell>` 支持 history，但没有稳定行身份或逐历史行版本 | 支持 history、tracked grid refs、idle compression 和增量 history snapshot |
| 主屏 + 备用屏 | 内部同时保留；公开 API 只能读取活动屏，需要小范围 fork | 内部同时保留；`inactive_grid` 私有，需要 fork | snapshot API 明确包含 primary/alternate extent |
| 模式 | 内部模型完整；公开已有 alt、mouse、bracketed paste 等部分 getter，其余需 snapshot view | `TermMode` 公开且覆盖 mouse/keypad/paste/alternate 等 | C API terminal/screen/effect 模型完整 |
| 查询回复 | 通过宿主 writer 返回 DA/DSR 等；实测 DSR 产生 PTY reply | 通过 `Event::PtyWrite`、`ColorRequest`、`TextAreaSizeRequest` 等交给宿主 | effects callback 含 `write_pty` 及宿主查询 |
| Resize/reflow | 主屏 reflow、备用屏 resize；实测 80x24 -> 40x30 通过 | 主屏 reflow、备用屏 resize；实测同样场景通过 | API 声明支持 resize/reflow |
| Snapshot/serialization | `Line` 等有可选 serde，但完整 `TerminalState` 无稳定序列化；必须导出 Astra schema | 默认 serde 只覆盖部分公开结构，saved cursor 被跳过且 inactive grid 私有；需较深 fork | snapshot record stream 最接近需求，支持 READY 后增量历史恢复 |
| 增量变化 | `Line::changed_since`、`get_changed_stable_rows` 可作为基础 | damage 主要面向当前 viewport，无历史行 seqno | render state 有全局和逐行 dirty；snapshot 支持分段恢复 |
| 许可证 | MIT | Apache-2.0 | MIT |
| 发布/供应链 | 官方 crate 未发布到 crates.io；必须 pin fork 和 lockfile | 官方 crate 可独立构建，依赖较小 | 官方明确 C API 尚未稳定；需要 Zig/C 构建链 |
| 结论 | **选择，受控最小 fork** | 淘汰 | 暂不采用，保留复评 |

### WezTerm 的实际缺口

上游不能不加修改直接满足 Astra：

1. `ScreenOrAlt`、primary screen、alternate screen 和 saved cursor 是私有状态，公开 `screen()` 只返回活动屏。
2. 完整模式、tab stops、scroll margins、palette override 等没有统一只读 snapshot view。
3. `StableRowIndex` 在 trim 后有用，但不是跨 reflow 的永久逻辑行 ID。
4. 上游 serde 不是兼容协议，也没有大小、版本和拒绝未知大对象的 wire 规则。
5. `term` 默认强依赖 image/kitty/sixel 路径和较大的 workspace 依赖树；Astra v1 服务端不应为未承诺的图像能力付出供应链和内存成本。

这些缺口都有局部边界，不要求重写 parser、grid 或 reflow，因此仍小于在 Alacritty 上补历史版本和逻辑身份的工作量。

### Alacritty 淘汰理由

Alacritty 的 headless 核心和 VT 覆盖都合格，但状态复制基础不足：

- 只有 active `grid()` 是公开的，备用屏切换后主屏历史仍存在却无法导出。
- damage tracking 面向 viewport 渲染，不提供历史行的单调版本。
- 没有能在 trim/reflow 后作为同步基础的行身份。
- serde 跳过部分运行态，不能作为完整 snapshot。

这意味着 Astra 必须侵入 grid、reflow、备用屏和序列化四层，fork 面明显大于 WezTerm。

### Ghostty 暂不采用理由

Ghostty 当前的 C API 在功能形态上最接近 Astra：它有完整 terminal effect callback、逐行 dirty、tracked grid reference，以及 READY/FINISH 分段 snapshot 和历史页恢复。但其官方头文件仍声明 `libghostty-vt` API 未稳定、可能发生 breaking change；当前仓库也没有稳定 tag 可作为长期 ABI 基线。本机没有 Zig，无法在本轮生成同工作负载的可比内存数据。

因此它不满足生产依赖的 API/工具链门禁。等 C ABI 与 snapshot format 有稳定版本、变更政策和可重复构建后，可重新运行本 ADR 的 workload；不能在此之前以 C shim 临时接入生产路径。

## 内存与 smoke 实验

### 工作负载

两个 Rust 候选使用相同逻辑 workload，均在 Apple Silicon macOS 上编译为 release：

1. 创建 8 个 80x24 terminal，history 上限 10,000 行。
2. 每个 terminal 写入 10,024 行带 SGR、ASCII、希腊字母和宽字符的短文本。
3. 验证 history 上限、模式切换、主/备用屏、DSR query reply。
4. resize 到 40x30，触发主屏 reflow。
5. 用 `/usr/bin/time -l` 比较 loaded 进程和同一 binary 空载进程的 maximum resident set size；差值除以 8。

| 指标 | WezTerm | Alacritty |
|---|---:|---:|
| `size_of::<Cell>()` | 24 B | 24 B |
| 空载 RSS | 1,654,784 B | 1,392,640 B |
| 8 terminal loaded RSS | 41,648,128 B | 172,277,760 B |
| 差值 | 39,993,344 B / 38.14 MiB | 170,885,120 B / 162.97 MiB |
| 每 terminal 近似增量 | 4.77 MiB | 20.37 MiB |

这是候选对比 workload，不是生产配额值。短行特别有利于 WezTerm 的 clustered line storage；全屏随机样式、hyperlink、长 grapheme 和图像会改变结果。`HIST-03` 仍必须按实际 heap bytes 设置硬上限，而不能用“行数 × 4.77 MiB”外推。

Ghostty 未给出伪造估计值：缺少 Zig 工具链，因此只完成内存机制审查，没有完成可比运行数据。API/发布稳定性已足以让它在本轮淘汰。

### 可复现边界

实验在临时 checkout 上完成，没有把候选源码或 benchmark binary 提交进 Astra：

- WezTerm 为最小 workspace checkout；`cargo check -p wezterm-term` 通过，headless smoke 通过。
- Alacritty 将 `alacritty_terminal` 核心作为独立 crate 编译；`cargo check` 和 headless smoke 通过。
- 两边 smoke 都直接喂 VT bytes，不启动 GUI 或 PTY；host reply 使用内存 writer/event sink 验证。
- 上述 commit、workload、结构大小和 RSS 是复现实验必须保持的输入/输出。正式 fork 落地后，等价 smoke 和内存回归必须进入 Astra CI；临时实验本身不构成生产依赖。

## Astra fork 的允许范围

fork 只允许以下架构性改动：

1. 增加只读 `AstraTerminalView`，一次性导出 primary、alternate、active screen、两个 cursor/saved cursor、modes、margins、tabs、palette、title、cwd、hyperlink table 和 parser continuation 所需状态。
2. 为逻辑行增加单调 `u64` 身份，并在 append、wrap、rewrap、trim、reset 和 alternate 生命周期中定义不变量；wrapped fragment 记录同一逻辑 ID 和 cell offset。
3. 暴露有界的 line/cell 迭代器与行 `SequenceNo`，不得提供无上限 clone-all API 给网络层。
4. 把终端 query reply 汇入 Astra 的 host effect sink；clipboard、notification、download、image 等能力必须经过显式安全策略。
5. 增加 `astra-headless` feature，关闭 Astra v1 未支持的 image/sixel/kitty graphics 依赖和代码；遇到这些序列安全忽略或返回受控的“不支持”，不得 panic。
6. 增加 fork conformance tests、上游 commit 记录和升级审计；不改变上游 VT 行为来迎合 Apple UI。

禁止的 fork 范围：

- 不在引擎里定义 protobuf 或网络 generation。
- 不把 ANSI renderer 当 snapshot exporter。
- 不把物理 `StableRowIndex` 直接暴露成永久 wire line ID。
- 不为 SwiftTerm 的现有 buffer 或滚动行为加入服务端特判。
- 不维护第二套权威 cell model。

## 后续任务门禁

`TERM-01` 完成后：

- `TERM-02` 可以定义 Astra Terminal State v2，但必须采用逻辑行 ID + cell offset，并包含 primary/alternate 与完整模式。
- `TERM-03` 只有在 `TERM-02` 完成后才能落地 fork 和服务端 `TerminalEngine`。
- `HIST-01` 不得直接复用 WezTerm `StableRowIndex` 作为协议 ID。
- `TERM-04` 仍被 `TERM-02`、`TERM-03` 和 `PROTO-01` 阻塞；客户端不得提前通过 ANSI replay 模拟完成。

## 上游证据

- WezTerm headless 入口与 `advance_bytes`：[term/src/lib.rs](https://github.com/wezterm/wezterm/blob/main/term/src/lib.rs)
- WezTerm scrollback、stable row、changed rows 与 reflow：[term/src/screen.rs](https://github.com/wezterm/wezterm/blob/main/term/src/screen.rs)
- WezTerm primary/alternate 与公开状态：[term/src/terminalstate/mod.rs](https://github.com/wezterm/wezterm/blob/main/term/src/terminalstate/mod.rs)
- WezTerm crate 依赖与 MIT metadata：[term/Cargo.toml](https://github.com/wezterm/wezterm/blob/main/term/Cargo.toml)
- Alacritty `Term`、modes、grid 与 resize：[alacritty_terminal/src/term/mod.rs](https://github.com/alacritty/alacritty/blob/master/alacritty_terminal/src/term/mod.rs)
- Alacritty host reply events：[alacritty_terminal/src/event.rs](https://github.com/alacritty/alacritty/blob/master/alacritty_terminal/src/event.rs)
- Alacritty core crate metadata：[alacritty_terminal/Cargo.toml](https://github.com/alacritty/alacritty/blob/master/alacritty_terminal/Cargo.toml)
- Ghostty C API stability notice：[include/ghostty/vt.h](https://github.com/ghostty-org/ghostty/blob/main/include/ghostty/vt.h)
- Ghostty snapshot API：[include/ghostty/vt/snapshot.h](https://github.com/ghostty-org/ghostty/blob/main/include/ghostty/vt/snapshot.h)
- Ghostty incremental render/dirty API：[include/ghostty/vt/render.h](https://github.com/ghostty-org/ghostty/blob/main/include/ghostty/vt/render.h)
