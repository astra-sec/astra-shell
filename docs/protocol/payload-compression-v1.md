# Payload compression v1

状态：Rust 实验分支已实现。Hello capability `payload.zstd` v1 只改变指定大 payload 的传输编码，不修改 Terminal State v2、文件内容或 QUIC 可靠性。Swift 尚未 offer，继续接收原文。

## 协商与范围

双方 offer/选择 `payload.zstd` v1 后，发送方才可使用 `PayloadEncoding::Zstd = 1`。`None = 0` 永远可用。不支持该能力的旧 peer 收到的 data 必须仍为原始字节，不能依赖 protobuf 忽略新字段来兼容压缩内容。

Rust runtime/semantic client 和默认 CLI 都 offer；默认 CLI 仅对文件传输使用压缩，终端仍走原有 ANSI 路径。managed gateway 把选择传入 WorkerStreamHello，worker 重新验证，压缩/解压发生在非特权用户 worker 中。

适用消息：

| 消息 | 压缩单位 | 新字段 |
|---|---|---|
| TerminalStateChunk | 整份 State 先独立压缩，再按最多 512 KiB 分片 | encoding=7、uncompressed_size=8 |
| HistoryPageChunk | 每个完整 HistoryPage 先独立压缩，再分片 | encoding=7、uncompressed_size=8 |
| WriteFileChunkRequest | 每个原始文件块，最多 1 MiB | encoding=5、uncompressed_size=6 |
| FileChunkResponse | 每个原始文件块，最多 1 MiB | encoding=5、uncompressed_size=6 |

Input、ACK、DATAGRAM、TerminalStateDiffChunk 和其他 RPC 不压缩。不把整个 QUIC stream 包在压缩流内。

## 编码、校验与上限

- None：data 为原文，uncompressed_size 必须为 0；protobuf 省略这些默认字段，保留原有 wire 格式。
- Zstd：data 为一个标准 zstd frame（State/Page 的 data 是其分片），uncompressed_size 是完整原文长度且非零。
- State/Page 的 total_size 始终是所有传输分片 data 长度之和；Zstd 时是压缩后长度。transfer ID、count、total_size、encoding、uncompressed_size、SHA-256 在所有分片上必须一致。
- SHA-256 始终覆盖原文。接收完整 payload 后有界解压，校验精确原文长度及 SHA-256，再 protobuf decode / schema validate / 原子发布；禁止把部分状态暴露给 renderer，完成后才 ACK。
- 不使用外部字典，也不跨关键帧、页面、文件块、终端或连接共享动态压缩上下文。单个标准 frame 外的尾随数据、拼接帧、skippable frame、未知 encoding、缺失协商、截断、非法元数据均拒绝。
- 压缩输入（在线路上的完整 payload）和解压输出均有硬上限：State 8 MiB、HistoryPage 4 MiB、文件块 1 MiB。不能通过压缩绕过原文上限。
- 解码器 window_log_max=23（8 MiB）；读取解压结果最多声明长度+1 字节，并检查精确相等。不得按未验证的 frame content size 任意分配内存。分片组装仍保留数量、总长度和重复片一致性检查。

## 文件续传

所有 offset、请求 length、文件 size、committed_offset、进度和 SHA-256 都针对原始文件。上传在解压/校验后进入原有 FileService；FileService 拒绝未解码的压缩块，以防把压缩表示落盘。下载客户端 API 返回已解码原文。

例如原始 1 MiB 块压成 200 KiB，成功写入后 committed_offset 增加 1 MiB。重连查询原始 offset，从那里生成新的独立块；不需要恢复压缩上下文。重试可以换压缩等级或直接发原文，幂等性比较原始字节而不是压缩表示。文件 snapshot、最终校验、原子提交规则不变。

上传校验由 FileService 在写盘前执行一次；下载由客户端传输 API 校验一次，CLI 不再重复计算同一块的 hash，最终文件校验仍保留。客户端遇到非法编码、越界或校验失败返回永久 FilePayloadError，CLI 停止传输并保留已有 partial file，不将其当成断网无限重连。

## 当前发送策略与执行调度

- zstd level 1，无字典。原文小于 1024 B 不尝试压缩。
- 只有节省至少 max(64 B, ceil(原文长度/20)) 才使用 zstd，否则原文回退。阈值是实现策略，不是接收端合法性规则；每块独立判断，不依赖扩展名。
- 编解码/校验在有并发限制的 blocking worker 上执行。每个进程分别为终端 payload 与文件 payload 保留一个许可，不让文件压缩占满 blocking pool；取消等待后，已开始工作仍持有许可直到结束。
- StreamingAttachment 持有待完成解码任务；next_event 被输入/resize 取消后，下一次调用继续等待同一任务，不能丢掉已读取关键帧。后续可靠消息（尤其 Exited）必须在此前关键帧解码后处理。
- 这些限制不等于全机 CPU 配额；managed 模式每个用户 worker 独立限制。实际交互延迟仍需结合多用户负载测量。

压缩降低传输字节，不改变 keyframe ACK gate、重连、repair、历史分页请求触发条件或超 MTU 的回退策略；不声称优化了逐 cell schema 本身。

格式依据：[RFC 8878](https://www.rfc-editor.org/rfc/rfc8878.html)。压缩不是加密；QUIC 仍保护内容，但传输长度可观察。独立帧避免跨对象共享上下文，不消除同一对象中秘密与攻击者可控内容共同压缩的长度侧信道。
