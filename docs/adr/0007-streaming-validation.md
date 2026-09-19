# Streaming v2 修复验证记录

日期：2026-09-05。基线：实验分支 `eccfb2f`，本记录针对其后的本地修复补丁。范围限 Rust、本机 loopback 和测试创建的临时 PTY；未更新 Swift、运行中的 App 或远端服务。

## 审查问题与回归覆盖

| 问题 | 修复与验证 |
|---|---|
| `select!` 取消可靠读取破坏帧边界 | `MessageReader` 持久化 header/payload 进度；逐字节边界取消后继续读取两帧 |
| epoch 切换拒绝合法迟到 ACK/repair | 区分退休 epoch；验证新 epoch pending 不被旧 ACK 清掉 |
| 单字符变化超过 MTU | sparse rows + cell splice；密集 viewport、宽字符/组合字符与越界拒绝测试 |
| 可靠关键帧积压、阻塞输入 | 一份未 ACK keyframe + 单个最新 pending；可靠写独立，字节/片段双重上限；慢 sink 回归 |
| live 导出遍历历史 | 直接导出 viewport、复用同一引擎更新的缓存；1000 行历史不进入 live state |
| 无法发送时采样限速失效 | 独立采样时间，生成 pending 也推进 16ms deadline |
| retry 发出的 generation 无法 ACK | retry 进入同一发送记录；ACK 后 pending 清空 |
| 乱序旧包覆盖单槽中的新包 | 以已建立 epoch + generation 高水位接收；100→99 和不同 epoch 均不能覆盖 |
| 缺乏实际使用入口与自动恢复 | `StreamingConnection` / `LiveTerminal` 后台驱动；`astra --streaming` 真实 PTY 验证 |

## 编码尺寸

每行填满 `cols - 1` 个 ASCII 字符，再改左上角一个字符；两个路由 ID 使用 canonical UUID 长度。实际 Quinn payload budget 为 1162 B。

| 窗口 | 修复前 delta | 修复后 delta | viewport keyframe |
|---|---:|---:|---:|
| 24×80 | 1494 B | 296 B | 18835 B |
| 40×120 | 2200 B | 306 B | 45005 B |
| 50×200 | 3242 B | 347 B | 95546 B |
| 60×180 | 3244 B | 339 B | 102398 B |

这是编码 payload，不包括 UDP/QUIC/IP 开销，不代表所有 TUI 的流量或性能。

## 真实传输与交互

- UDP 中继每 11 包丢一包，交替延迟 5/30 ms 造成乱序，另加入双向完全断网；两个终端在一次共享连接恢复后继续输入输出。
- UI 不消费事件超过 15 秒租约 TTL，后台依旧 ACK/续租；恢复时离线输入被拒绝，最新 resize 保留。
- 编译出的 CLI 在真实 PTY 中运行：输入回显、31×103 → 42×132 的 SIGWINCH、远端 `stty size` 校验及正常退出。
- 原始可靠语义同步、旧客户端协商、managed worker bridge、历史分页等原测试继续通过。撤回的实验 v1 不与 v2 混用。

## 复现

```sh
cargo test --offline --all-targets -- --test-threads=4
cargo clippy --offline --all-targets -- -D warnings
cargo build --offline --bins
cargo test --offline --lib dense_viewports_one_cell_delta -- --nocapture
```

压缩接入前的全量结果：139 个 library tests、4 个 astra CLI tests、2 个 astrad tests、1 个真实 PTY integration test，共 146 项通过。

## 边界

超 MTU 的大幅变化仍走有 ACK gate 的可靠关键帧，受 RTT/带宽限制；没有宣称实现可取消、独立传输流的关键帧替换。历史分页 API 可用，但实验 CLI 尚无独立 scrollback 浏览 UI，且要求交互 TTY。Swift 仍未协商本能力。能力名为 `terminal.datagram_state` v2，试用必须配套本分支的 astrad；默认 CLI 不变。
