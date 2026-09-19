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

## Zstd 接入验证（2026-09-05）

协议见 `../protocol/payload-compression-v1.md`。现在已实现 `payload.zstd` v1 的关键帧、历史页和双向文件块编码，不只是离线压缩实验。上方编码尺寸表中的 keyframe 数字是压缩前 State protobuf 本体。

- 151 项 library tests 通过；默认跳过的 1 项手动 release benchmark 单独执行通过。
- astra CLI 5 项（包括新增永久 payload 错误不能无限重连）、astrad 2 项、真实 PTY 集成 1 项通过，共 159 项功能测试。CLI 新增用例在全量测试后单独运行 `cargo test --offline --bin astra` 验证。
- `cargo clippy --offline --all-targets -- -D warnings`、`cargo build --offline --bins`、`git diff --check` 和 protoc schema 编译通过。
- 真实 loopback QUIC 上验证压缩上传/下载、上传与下载过程中分别断开连接后恢复、重复已提交块、空 EOF，以及未 offer zstd 时收到原文。
- FileService 重建后恢复原始 committed offset；已压缩上传块可以改成原文重试，结果一致。非法压缩/错误 SHA 不推进上传进度。
- 跨片关键帧原子组装、历史页独立解码、原格式 wire 字节完全不变、协商/worker 校验、截断/尾随/拼接/未知编码/错误 hash/解压长度或 window 超限拒绝均有测试。
- 后台解码等待取消后继续同一任务；取消文件任务后许可保留到实际工作结束，文件队列不占用终端队列的许可。

### Release codec 微基准

本机 Apple M1 Pro，zstd 1.5.7 / Rust zstd 0.13.3，level 1；每项预热 20 次、记录 200 次，取 p50/p95。原文复制不计时；encode 包括尝试压缩与原文回退；decode 包括 SHA-256。**不包括 protobuf 解码/终端 validator、任务调度、渲染或网络，不是端到端交互延迟基准。**

| 合成样本 | 原文 B | 实际 payload B | encode p50 / p95 μs | decode+SHA p50 / p95 μs |
|---|---:|---:|---:|---:|
| 50×200 混合 ASCII 关键帧 | 95,550 | 14,410 | 174 / 223 | 371 / 480 |
| 1 MiB 重复 x 文件块 | 1,048,576 | 51 | 123 / 194 | 3,318 / 3,480 |
| 1 MiB 伪随机文件块 | 1,048,576 | 1,048,576（原文回退） | 158 / 219 | 2,991 / 3,079 |

重复 x 是极易压缩样本，不能外推普通文件的压缩率；随机块回退仍有尝试压缩的 CPU 成本。传输封装/QUIC/UDP/IP 不在 payload 数字内。原文 hash 在文件存储/客户端传输层各执行一次，CLI 不再重复校验同一下载块，最终文件校验仍保留。

复现：`cargo test --offline --release --lib payload_codec_benchmark -- --ignored --nocapture`。

## 边界

超 MTU 的大幅变化仍走有 ACK gate 的可靠关键帧，受 RTT/带宽限制；没有宣称实现可取消、独立传输流的关键帧替换。历史分页 API 可用，但实验 CLI 尚无独立 scrollback 浏览 UI，且要求交互 TTY。Swift 仍未协商本能力。能力名为 `terminal.datagram_state` v2，试用必须配套本分支的 astrad；默认 CLI 不变。
