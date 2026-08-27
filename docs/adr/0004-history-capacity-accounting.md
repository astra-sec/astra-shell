# ADR 0004: 权威历史容量与字节计量

- 状态：已接受
- 日期：2026-08-27
- 任务：HIST-03

## 背景

HIST-01/02 已使主屏 viewport、已保留历史、稳定 Anchor 和分页共用同一权威 `Screen`。OPS-01 又为每个 Terminal 预留了默认 8 MiB history capacity，但引擎仍只使用临时的 2,000 行常量；reservation 尚未约束真实历史模型。

原计划要求每个 Terminal 默认保留 10,000 行或 8 MiB，取先达到者，允许配置更大值并保留服务端硬上限。单独提高行数仍会让宽行、复杂 grapheme、style 和 hyperlink 绕过内存边界，因此不能作为实现。

## 决策

### 唯一计量位置

历史容量由受控 `astra-wezterm-term::Screen` 计量。它是压缩 `Line`、逻辑行身份、viewport 和 trim 的唯一所有者；TerminalManager、协议层和 Apple 客户端不建立第二份字节计数，也不从 ANSI 或分页编码大小推测服务端内存。

只计量 primary screen 中严格位于 viewport 之前的 retained history。当前 viewport 属于 OPS-01 的 Terminal base-memory capacity；alternate screen 永远没有 history。

### Accounted bytes

每条 retained history row 的 accounted bytes 是引擎当前存储表示的保守费用：

- `Line` 与 `AstraLineIdentity` 的固定结构；
- line zone、cell vector、压缩 text、attribute cluster 和 wide-cell bitset 的已分配存储；
- heap grapheme、fat cell attributes 与 hyperlink URI/parameters；共享 hyperlink 在每个引用它的 cluster/cell 上重复计费，避免共享关系成为绕过上限的手段。

正常上滚前，row 先执行与现有引擎相同的 `compress_for_scrollback`，再按压缩后的真实表示计费。计数覆盖权威模型保留的数据，但不是操作系统 RSS/cgroup 指标；allocator/container spare capacity 由 Terminal base-memory reservation 承担，后续 OPS-02 可独立暴露 RSS 指标。

### 增量维护与重算

- 普通 full-screen scroll 只对新进入历史和被淘汰的前缀增减计数，开销与本次滚动行数相关，不扫描全部 10,000 行。
- resize/reflow 会改变物理行和压缩表示，因此完成 reflow 后从同一 `Line` 队列重算一次。
- erase/reset 清零历史计数。
- debug/test invariant 将记录值与权威队列重算值比较；业务层不缓存另一份历史内容。

### 双上限 trim

默认 `max_rows = 10_000`、`max_bytes = 8 MiB`。每次新增历史或 resize/reflow 后，只要任一条件成立，就从 primary history 最旧前缀开始删除：

```text
history_rows > max_rows || accounted_history_bytes > max_bytes
```

trim 不删除当前 viewport，不影响 alternate screen，也不回收或重用 `logical_line_id`。正常 trim 不轮换 epoch；保留行继续使用原 Anchor，分页的 `oldest_available` 自然前移。请求已经被 trim 的 Anchor 返回现有 HIST-02 定义的空正常页，而不是 ANSI 重放或客户端估算。

### 配置与硬限制

`ResourcePolicy` 同时携带 per-Terminal history rows 与 bytes。`astrad serve` 暴露：

- `--terminal-history-rows`，默认 10,000；
- `--terminal-history-mib`，默认 8。

managed gateway 必须把两项值原样传给内部 worker。网络客户端不能声明或提高容量。服务端拒绝 0，以及超过 1,000,000 rows 或 1 GiB bytes 的单 Terminal 配置；per-user/global history capacity 仍必须容纳一个 per-Terminal byte reservation。

提高 rows 但不提高 bytes 不保证保留更多复杂内容；提高 bytes 但不提高 rows 也不会超过行上限。这是有意的双重边界。

## 兼容与协议

- 不修改 wire schema 或 capability；HistoryPage 已携带真实 oldest/newest Anchor。
- semantic 与 N/N-1 ANSI serializer 都从同一已 trim 权威 State 读取；不新增 COMPAT/PATCH。
- Apple 客户端仍只按服务端 Anchor 合并和裁剪分页，不需要知道服务端 accounted-byte 公式。

## 验收

- 默认简单输出能保留超过旧 2,000 行，并在第 10,001 条历史行进入时确定性淘汰最旧行。
- 小 byte limit 下，复杂 Unicode/style/hyperlink 在达到 row limit 前触发 trim，记录值始终不超过 byte limit。
- resize/reflow 后重新计量且仍满足两个上限；保留逻辑行的 Anchor 不因正常 trim 被重写。
- alternate screen 不消耗 history budget，不改变 primary history。
- 分页 oldest/newest、`more_before` 和被 trim Anchor 的行为与引擎队列一致。
- rootless 与 managed worker 获得相同 rows/bytes policy；非法 CLI policy 在创建 PTY 前失败。

## 不在本 ADR 范围

- HIST-04 的滚动条 UI；
- 历史持久化到磁盘；
- SYNC-01/02 的 diff/ACK；
- OPS-02 的 doctor、metrics、qlog 和进程 RSS；
- OS/cgroup 级强制内存限制。
