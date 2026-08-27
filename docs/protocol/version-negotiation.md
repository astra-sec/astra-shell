# Astra application protocol negotiation

状态：`PROTO-01`。当前 application protocol `N = 2`，支持 `N-1 = 1`。QUIC ALPN 暂时保持 `astra/1`：ALPN 表示 framing/authentication family，Hello 中的 application version 表示可演进的消息语义。

## Hello 兼容规则

`ClientHello.protocol_version` 是为旧 server 保留的 exact-version hint。新 client 必须：

- 把 `protocol_version` 保持为 1，使只认识字段 1/2 的 v1 server 仍会接受；
- 用 `minimum_protocol_version`/`maximum_protocol_version` 广告真实范围，当前是 `1...2`；
- 以 `CapabilityOffer(name, min, max)` 广告已实现的能力范围。

新 server 收到没有 min/max 的旧 Hello 时，把 `protocol_version` 当成 exact range。收到新 Hello 时选择双方重叠范围的最高 application version。没有重叠必须在认证前失败。

`ServerHello.protocol_version` 是选择结果。server 只能返回 client offer 过的 capability，版本必须位于双方区间。client 必须原子校验整个选择；重复、未 offer、version 0、越界或未知选择均终止连接。

上限是 64 个 capability、名称 64 ASCII bytes。名称只能含小写字母、数字、`.`、`_`。server 忽略不认识的 client offer；client 不接受 server 凭空选择的能力。

## Capability registry

| 名称 | v1 语义 | 当前 runtime 是否 offer | 开启条件 |
|---|---|---|---|
| `terminal.legacy_ansi_snapshot` | 旧 `TerminalSnapshot` + ANSI replay | 是 | 迁移期保留 |
| `terminal.semantic_state` | `astra.terminal.v2.State` 语义状态 | 否 | `TERM-03`、`TERM-04` 完成并有双端测试 |
| `terminal.history_paging` | 使用 v2 `Anchor` 的历史分页 | 否 | `HIST-02` 完成 |
| `terminal.datagram_state` | generation 累计 patch 可走 QUIC DATAGRAM | 否 | `SYNC-01` 至 `SYNC-03` 完成 |

Capability 名称存在不代表实现完成。runtime support list 只能加入已经通过对应架构任务验收的能力；禁止为了让 UI 走新分支而提前 offer。

## N/N-1 行为矩阵

| Client | Server | 结果 |
|---|---|---|
| v2 | v2 | 选择 application v2，选择已实现 capability 交集 |
| v2 | v1 | v1 server 读取 legacy hint 1、忽略新增字段并返回 v1；v2 client 接受 v1、无 capability |
| v1 | v2 | v2 server 将旧字段解释为 exact v1，返回 v1、无 capability；v1 client 正常工作 |
| 低于 v1 / 高于 v2 且无重叠 | v2 | 认证前拒绝 |

测试必须同时覆盖 wire unknown-field 行为和语义选择，不能只测试 protobuf 能 decode。

## 字段弃用时间表

| 字段/路径 | v2 状态 | 停止发送条件 | 最早删除 wire tag |
|---|---|---|---|
| `AttachResponse.history` | deprecated fallback | `terminal.semantic_state` + `terminal.history_paging` 上线，所有受支持 client 已跨过 N-1 窗口 | application v4；删除时 reserve tag 4/name |
| `TerminalSnapshot.contents/normal_contents` | deprecated fallback | `TERM-03`、`TERM-04` 完成，v2 semantic capability 稳定一个 release | application v4；删除 message 前 reserve 所有 tag/name |
| `TerminalEvent.output` raw PTY | 迁移期主路径 | `SYNC-02` reliable semantic diff 覆盖，真实 TUI matrix 通过 | application v4；删除时 reserve tag 10/name |
| `TerminalEvent.snapshot` legacy ANSI | deprecated fallback | 同 `TerminalSnapshot` | application v4；删除时 reserve tag 14/name |

规则：一个字段在 N 标记 deprecated 后，至少完整支持 N/N-1 两个版本并经过一个发布窗口，才可在 N+2 停止发送；wire tag/name 最早在 N+2 schema 中 `reserved`，永不复用。实际删除还必须满足 `PROJECT_STATUS.md` 对应 PATCH/COMPAT 的移除条件，两者取更晚者。

## 安全与降级

- capability negotiation 在 TLS 保护的 QUIC stream 内、用户认证前完成；失败不得降级重试为猜测模式。
- 未选择 semantic capability 时只能走已登记的 legacy compatibility path，不能混发 v2 state。
- 未选择 DATAGRAM capability 时状态全部走可靠 stream；不能“试发再 fallback”。
- server 日志只记录 application version 和 capability 数量/名称，不记录终端内容、challenge、key 或 token。
