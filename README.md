# Astra Shell MVP

Astra Shell 是一个以 QUIC 连接多个持久 PTY 的远程终端原型。`astrad` 支持两种部署：个人使用的 rootless 单用户 daemon，以及类似系统 sshd 的多用户 gateway。`astra` 是配套 CLI 客户端。

当前 MVP 已实现：

- QUIC/TLS 1.3，ALPN 为 `astra/1`；
- SSH 风格的主机证书 TOFU：首次连接确认，后续自动校验并拒绝证书变化；
- OpenSSH Ed25519/RSA 私钥、SSHSIG challenge-response 和 `authorized_keys`；
- 客户端选择目标 Unix 用户，签名同时绑定用户名、服务实例和随机 challenge；
- managed gateway 按 passwd 数据库查找用户及其 `~/.ssh/authorized_keys`；
- 每个 UID 独立的降权 worker、Unix socket 和 PTY；
- root gateway 不创建 PTY，只代理认证后的协议字节流；
- 一条 QUIC 连接上的多个独立双向请求流；
- 创建、列出、附着和关闭多个 PTY；
- Terminal 内部使用 UUID 作为 canonical ID，同时提供当前用户 worker 内从 1 开始、只增不复用的短 ID；
- Linux 默认 Shell 从 `/etc/os-release`、`/proc/sys/kernel/osrelease` 和服务端架构直接生成单行系统欢迎信息，不执行 MOTD 脚本；
- 每个新 PTY 传递客户端 `TERM` 与白名单 `LANG`/`LANGUAGE`/`LC_*`，不可用时回退服务端 UTF-8 locale，并开启 `IUTF8` 输入处理；
- 可靠、有序的输入、输出和 resize；
- 单写者输入租约及 fencing ID、命令序列号；
- 客户端断开后 PTY 继续运行；交互客户端会自动重连、重新认证并恢复最近 1 MiB 原始输出；
- Astra Files/1 文件协议：目录分页、元数据、创建/删除/重命名，以及带 SHA-256、断点续传和原子提交的上传下载；
- 活动 Terminal 只以内存中实际持有的 PTY 为准，不保存可能失真的 `running` 记录；
- QUIC keepalive、15 秒认证超时，以及 gateway/worker 单实例进程锁；
- managed worker 在没有活动 Terminal、没有请求并连续空闲 10 分钟后自动退出；
- 工作目录边界，子进程 cwd 不能逃出服务端配置的 `session-root`。

## 构建

下面的设置会让 Cargo 缓存、构建产物和测试数据全部留在仓库中：

```bash
cd /home/mimi/astra-shell
export CARGO_HOME="$PWD/.cargo-home"
export CARGO_TARGET_DIR="$PWD/target"
cargo build --bins
```

## 初始化凭据

```bash
./target/debug/astrad init --state-dir state
ssh-keygen -t ed25519 -N '' -f state/id_ed25519
install -m 600 state/id_ed25519.pub state/authorized_keys
```

也可以使用 RSA；Astra 的 RSA SSHSIG 使用 `rsa-sha2-512`，不会回退到旧的 SHA-1
`ssh-rsa` 签名：

```bash
ssh-keygen -t rsa -b 3072 -N '' -f state/id_rsa
install -m 600 state/id_rsa.pub state/authorized_keys
```

服务端证书、私钥、`authorized_keys` 和客户端测试密钥都在 `state/` 中。服务端状态目录会设置为 `0700`，秘密文件设置为 `0600`。`host-cert.der` 是服务端主机身份的一部分，客户端不再需要复制或显式传入它。

## Rootless 单用户模式

Rootless 模式使用 `state/authorized_keys`，daemon 只能代表启动它的 Unix 用户。

### 启动服务端

```bash
./target/debug/astrad serve \
  --listen 127.0.0.1:4433 \
  --state-dir state \
  --session-root /home/mimi/astra-shell
```

`astrad` 默认以前台模式运行，适合由 systemd、launchd 或其他进程管理器托管。

### 资源配额

Rootless 与 managed 模式使用同一套分层 `ResourceGovernor`。默认每用户允许 8 个认证连接、256 条活动 Stream、64 个 Terminal、256 个 Attachment、256 MiB Terminal 基础内存容量、512 MiB history 容量、256 个活动文件操作、16 个活动 upload 和 8 GiB 声明 upload bytes。每个 Terminal admission 至少预留 4 MiB 基础内存和 8 MiB history；大初始网格会增加基础内存 claim。

所有限制都能从 `astrad serve --help` 中的 `--max-global-*`、`--max-user-*` 和 `--terminal-*` 参数覆盖。例如：

```bash
./target/debug/astrad serve \
  --listen 127.0.0.1:4433 \
  --state-dir state \
  --session-root /home/mimi/astra-shell \
  --max-user-terminals 32 \
  --max-user-history-mib 1024 \
  --terminal-history-mib 16
```

多个维度同时生效，实际可创建数量由最先耗尽的维度决定。0 不表示无限，零值或 global 无法容纳一个完整 user capacity 的配置会在 daemon 启动前失败。超限返回稳定的 `quota` 错误，只拒绝新资源，不结束已经运行的 Terminal。managed gateway 按 UID 预留完整 user worker capacity，root gateway 不解析 Terminal 内容；内部 worker 不能接受客户端自报或提高的配额。完整所有权与默认值见 [`docs/adr/0003-resource-quota-model.md`](docs/adr/0003-resource-quota-model.md)。

### 启动客户端

连接参数采用 SSH 风格的 `[USER@]HOST`、`-p PORT`、`-l USER` 和 `-i IDENTITY`。不写 `-i` 时依次尝试 `~/.ssh/id_ed25519` 和 `~/.ssh/id_rsa`；不写用户时使用 `USER`/`LOGNAME`。下面的开发凭据位于项目 `state/`，所以显式使用 `-i`：

```bash
./target/debug/astra \
  -p 4433 \
  -i state/id_ed25519 \
  mimi@127.0.0.1 list
```

第一次连接会像 SSH 一样显示服务端 X.509 证书的 SHA-256 指纹，并要求输入 `yes`；确认后记录到 `~/.config/astra/known_hosts`。以后同一主机和端口的证书发生变化会直接拒绝连接，而且这个检查发生在发送用户名和用户签名之前。Astra 使用独立文件，不会把 TLS 证书条目混入 OpenSSH 的 `~/.ssh/known_hosts`。

不带子命令时，与 `ssh user@host` 一样直接创建并附着默认 shell：

```bash
./target/debug/astra -p 4433 \
  -i state/id_ed25519 \
  mimi@127.0.0.1
```

也可以显式创建命名终端，或直接执行 argv（不经过 `$SHELL -c`）：

```bash
./target/debug/astra -p 4433 \
  -i state/id_ed25519 \
  mimi@127.0.0.1 new --name logs -- /usr/bin/tail -f README.md
```

`new` 默认输出面向用户的短 ID。`list` 同样默认显示短 ID；需要诊断或为客户端保存稳定身份时使用 `list --long` 查看 canonical UUID：

```bash
./target/debug/astra -p 4433 -i state/id_ed25519 mimi@127.0.0.1 list
./target/debug/astra -p 4433 -i state/id_ed25519 mimi@127.0.0.1 list --long
```

重新附着：

```bash
./target/debug/astra -p 4433 \
  -i state/id_ed25519 \
  mimi@127.0.0.1 attach 1
```

`attach` 和 `close` 同时接受短 ID 与 UUID。短 ID 只用于当前 host、Unix 用户和 worker 生命周期内的人工选择；客户端缓存与自动重连必须保存 UUID，避免 worker 重启后短 ID 重新从 1 开始时误附着到另一个 Terminal。

无人值守的首次连接可以显式使用 SSH 同名策略；它只接受并记录新主机，绝不会覆盖已经变化的证书：

```bash
./target/debug/astra -p 4433 \
  -o StrictHostKeyChecking=accept-new \
  -i state/id_ed25519 \
  mimi@127.0.0.1 list
```

支持的 SSH 风格选项是 `StrictHostKeyChecking=yes|ask|accept-new|no` 和 `UserKnownHostsFile=PATH`。默认是 `ask`。需要由配置管理系统严格下发证书时，仍可使用 `--server-cert /path/to/host-cert.der`，此时执行标准 TLS 证书和名称校验，不使用 TOFU。

创建终端时，客户端会像 SSH 一样发送当前 `TERM` 和 locale。服务端只接受固定白名单中的 `LANG`、`LANGUAGE` 和标准 `LC_*`，不会接收 `PATH`、动态链接器或任意环境变量。客户端 locale 在服务端不存在或不是 UTF-8 时，服务端使用 `C.UTF-8`/`C.utf8` 等可用 UTF-8 locale；如果服务端完全没有 UTF-8 locale，则拒绝创建 PTY，并返回明确错误。

交互附着时按 `Ctrl+]` 只分离客户端，不结束远端进程。`--read-only` 创建观察者；已有写入者时，可显式使用 `--takeover` 获取新的 fencing lease。

连接中断时，`astra` 会按 250 ms 到 5 s 的退避间隔持续重连，并使用 canonical UUID 和服务端签发的 opaque resume token 恢复原来的写入权。每次恢复都会轮换 fencing lease ID，并从序列号 1 重新开始；旧连接迟到的命令和清理动作因此不能影响新连接。客户端会用服务端保存的有界 history 重建本地终端画面。由于断线瞬间无法可靠判断最后一次输入是否已经送达 PTY，Astra 不会自动重放离线输入，以免命令被执行两次。

## Astra Files/1

文件操作复用终端所在的同一条认证 QUIC 连接，但每个请求使用独立、低于终端优先级的双向 Stream。它不是 SFTP：协议以稳定传输 ID、幂等 offset chunk 和文件快照为核心，因此 QUIC 连接完全失效后仍能在新连接上恢复。

```bash
# 查看能力、目录和元数据
astra -p 4433 mimi@HOST files capabilities
astra -p 4433 mimi@HOST files ls .
astra -p 4433 mimi@HOST files stat README.md

# 上传、下载和基本管理
astra -p 4433 mimi@HOST files put ./local.tar remote.tar
astra -p 4433 mimi@HOST files get remote.tar ./downloaded.tar
astra -p 4433 mimi@HOST files mkdir artifacts
astra -p 4433 mimi@HOST files mv remote.tar artifacts/remote.tar
astra -p 4433 mimi@HOST files rm artifacts/remote.tar
```

目标已存在时默认拒绝覆盖；`put`、`get` 和 `mv` 可显式传 `--overwrite`。上传先写入目标目录中的私有 `.astra-upload-<transfer-id>.part`，每块校验 SHA-256；重连时客户端用相同 transfer ID 查询服务端实际 committed offset。完成后服务端校验整个文件、执行 `fsync` 并在同一目录内原子 rename。

下载写入本地 `<name>.astra-part`，并保存对应 snapshot sidecar。客户端或网络中断后，只有远端文件的 inode、大小和修改时间快照仍一致时才继续；所有块和最终文件都会校验 SHA-256。远端文件发生变化时停止续传，不会把两个版本拼接在一起。

远端路径限制在 `astrad --session-root` 内，拒绝 `..` 和通过符号链接逃出根目录；文件操作在 rootless daemon 当前 UID 或 managed 模式的非特权用户 worker 中执行，不在 root gateway 中执行。Unix 路径在线路上使用 bytes，因此非 UTF-8 文件名仍可寻址。

## Managed 多用户模式

Managed 模式使用系统 passwd 数据库和每个用户自己的 SSH 授权文件。真正服务多个 UID 时，gateway 必须由 root 启动：

```bash
sudo /home/mimi/astra-shell/target/debug/astrad init \
  --state-dir /home/mimi/astra-shell/state-managed
sudo /home/mimi/astra-shell/target/debug/astrad serve \
  --managed \
  --listen 0.0.0.0:4433 \
  --state-dir /home/mimi/astra-shell/state-managed
```

请为 managed 模式使用单独、由 root 初始化的状态目录；gateway 会拒绝非 daemon UID 所有、group/other 可写的状态目录，以及非 `0600` 的主机私钥。正式部署还应把 `astrad` 放在 root 管理且普通用户不可修改的可执行路径。上面的路径只是保持本项目开发产物都在工作目录内的本机示例。

客户端选择目标账户：

```bash
./target/debug/astra \
  -p 4433 \
  alice@SERVER_IP
```

这里会自动选择本机的 `~/.ssh/id_ed25519`，不存在时再尝试 `~/.ssh/id_rsa`。需要指定另一把密钥时使用与 SSH 相同的 `-i /path/to/key`；`-l alice SERVER_IP` 与 `alice@SERVER_IP` 等价。

默认授权文件与 OpenSSH 一致：

```text
/home/alice/.ssh/authorized_keys
/home/alice/.ssh/authorized_keys2
```

gateway 会检查 HOME、`.ssh` 和授权文件的所有者，拒绝符号链接以及 group/other 可写路径。认证成功后，它按目标账户的 supplementary groups、GID、UID 顺序降权启动 worker。运行状态按 UID 隔离：

```text
state/users/1001/session.sock
state/users/1001/worker.pid
state/users/1002/session.sock
state/users/1002/worker.pid
```

用户 worker 不随 gateway 退出，因此 gateway 可以滚动重启而不关闭用户 PTY。没有活动 Terminal 和请求时，worker 默认连续空闲 600 秒后自动退出；使用 `--worker-idle-timeout-seconds SECONDS` 调整，设为 `0` 可关闭回收。非 root 也可以启动 `--managed` 做测试，但只能登录当前 UID，不能切换到其他账户。

### systemd 服务

仓库提供了 [`contrib/systemd/astrad.service`](contrib/systemd/astrad.service)。不要让 root 服务直接执行普通用户可修改的 `target/debug/astrad`；先安装一份 root 所有的可执行文件，再安装 unit：

```bash
cargo build --bin astrad
sudo install -o root -g root -m 0755 target/debug/astrad /usr/local/sbin/astrad
sudo install -o root -g root -m 0644 \
  contrib/systemd/astrad.service /etc/systemd/system/astrad.service
sudo systemctl daemon-reload
sudo systemctl enable --now astrad.service
```

查看状态和日志：

```bash
systemctl status astrad.service
journalctl -u astrad.service -f
```

unit 的 `KillMode=process` 是 managed 模式的持久会话语义所必需的：停止或重启 gateway 时只终止主进程，不误杀已经降权且持有 PTY 的用户 worker。修改 unit 中的监听地址、状态目录或工作目录后，需要执行 `sudo systemctl daemon-reload && sudo systemctl restart astrad.service`。

`list` 只返回对应 worker 当前实际持有且仍在运行的 Terminal。进程退出后，最终输出会在内存中短暂保留以完成已发起的 attach，随后记录被清理；worker 或 rootless daemon 重启后不会从磁盘恢复历史 Terminal。

`--authorized-keys-dir DIR` 是测试/集中式密钥目录选项，此时 gateway 读取 `DIR/USERNAME`；生产默认应沿用各用户的 `~/.ssh/authorized_keys`。

## 测试

```bash
export CARGO_HOME="$PWD/.cargo-home"
cargo test
cargo clippy --all-targets -- -D warnings
./scripts/local-smoke.sh
./scripts/managed-smoke.sh
```

两个 smoke test 都只在 `.local-test/` 内生成一次性服务端、SSH 密钥和 Astra known-hosts。它们会验证首次 `accept-new`、后续严格主机校验以及显式证书 pin。Rootless 测试还会以一个 Terminal 的低配额重启 daemon，验证第二个 Terminal 收到 `quota` 且已 admission 的 Terminal 不受影响。Managed 测试会验证目标 UID、跨账户拒绝、UTF-8 locale fallback、`TERM`/`IUTF8` 以及 gateway 重启后 PTY 恢复。

## MVP 边界

这还不是完整产品：

- managed 模式已经按 Unix 账户、supplementary groups、GID 和 UID 隔离，但尚未接入 PAM、账户锁定/过期策略；
- 已实现认证后连接、Stream、worker、Terminal/Attachment、容量和文件/upload 配额；尚未实现认证前按来源地址的速率限制、Retry 和审计日志后端，公开暴露前仍需补齐并接受独立安全审计；
- 认证兼容 OpenSSH Ed25519/RSA 密钥格式和 `authorized_keys`，客户端会自动选择 `~/.ssh/id_ed25519` 或 `~/.ssh/id_rsa`，但暂不支持 ssh-agent、加密私钥、ECDSA、SSH 用户证书及 authorized_keys options；
- QUIC 主机身份已经支持独立的 SSH 式 TOFU 文件，但当前 pin 的是完整自签名证书；正式的证书轮换机制尚未实现；
- 服务端维护唯一权威语义 screen/grid/history 并向新客户端发送 State v2；原始输出/ANSI snapshot 只保留为登记的 N/N-1 兼容路径；
- 暂无 QUIC DATAGRAM 累计状态同步、预测和端口转发；Astra Files/1 已支持单文件传输和基本目录操作，但尚未提供递归目录同步、稀疏文件、ACL/xattr 和 GUI；
- 暂无 SSH stdio fallback；
- rootless 模式的 PTY 仍由 gateway 进程持有；managed 模式已经使用可跨 gateway 重启存活、空闲时自动回收的独立用户 worker，但尚未提供正式的 worker 升级管理命令。

线协议定义见 [`proto/astra.proto`](proto/astra.proto)，总体产品方向见 [`quic-mosh-tmux-implementation-plan.md`](quic-mosh-tmux-implementation-plan.md)。
