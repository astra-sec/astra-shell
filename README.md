# Astra Shell MVP

Astra Shell 是一个以 QUIC 连接多个持久 PTY 的远程终端原型。`astrad` 支持两种部署：个人使用的 rootless 单用户 daemon，以及类似系统 sshd 的多用户 gateway。`astra` 是配套 CLI 客户端。

当前 MVP 已实现：

- QUIC/TLS 1.3，ALPN 为 `astra/1`；
- SSH 风格的主机证书 TOFU：首次连接确认，后续自动校验并拒绝证书变化；
- OpenSSH 私钥、SSHSIG challenge-response 和 `authorized_keys`；
- 客户端选择目标 Unix 用户，签名同时绑定用户名、服务实例和随机 challenge；
- managed gateway 按 passwd 数据库查找用户及其 `~/.ssh/authorized_keys`；
- 每个 UID 独立的降权 worker、Unix socket、SQLite 和 PTY；
- root gateway 不创建 PTY，只代理认证后的协议字节流；
- 一条 QUIC 连接上的多个独立双向请求流；
- 创建、列出、附着和关闭多个 PTY；
- 每个新 PTY 传递客户端 `TERM` 与白名单 `LANG`/`LANGUAGE`/`LC_*`，不可用时回退服务端 UTF-8 locale，并开启 `IUTF8` 输入处理；
- 可靠、有序的输入、输出和 resize；
- 单写者输入租约及 fencing ID、命令序列号；
- 客户端断开后 PTY 继续运行，重新附着时恢复最近 1 MiB 原始输出；
- SQLite 保存 Terminal 元数据和退出状态；
- QUIC keepalive、15 秒认证超时，以及 gateway/worker 单实例进程锁；
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

服务端证书、私钥、`authorized_keys`、SQLite 和客户端测试密钥都在 `state/` 中。服务端状态目录会设置为 `0700`，秘密文件设置为 `0600`。`host-cert.der` 是服务端主机身份的一部分，客户端不再需要复制或显式传入它。

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

### 启动客户端

连接参数采用 SSH 风格的 `[USER@]HOST`、`-p PORT`、`-l USER` 和 `-i IDENTITY`。不写 `-i` 时自动使用 `~/.ssh/id_ed25519`；不写用户时使用 `USER`/`LOGNAME`。下面的开发凭据位于项目 `state/`，所以显式使用 `-i`：

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

重新附着：

```bash
./target/debug/astra -p 4433 \
  -i state/id_ed25519 \
  mimi@127.0.0.1 attach TERMINAL_UUID
```

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

这里会自动选择本机的 `~/.ssh/id_ed25519`。需要指定另一把密钥时使用与 SSH 相同的 `-i /path/to/key`；`-l alice SERVER_IP` 与 `alice@SERVER_IP` 等价。

默认授权文件与 OpenSSH 一致：

```text
/home/alice/.ssh/authorized_keys
/home/alice/.ssh/authorized_keys2
```

gateway 会检查 HOME、`.ssh` 和授权文件的所有者，拒绝符号链接以及 group/other 可写路径。认证成功后，它按目标账户的 supplementary groups、GID、UID 顺序降权启动 worker。运行状态按 UID 隔离：

```text
state/users/1001/session.sock
state/users/1001/astra.db
state/users/1002/session.sock
state/users/1002/astra.db
```

用户 worker 不随 gateway 退出，因此 gateway 可以滚动重启而不关闭用户 PTY。非 root 也可以启动 `--managed` 做测试，但只能登录当前 UID，不能切换到其他账户。

`--authorized-keys-dir DIR` 是测试/集中式密钥目录选项，此时 gateway 读取 `DIR/USERNAME`；生产默认应沿用各用户的 `~/.ssh/authorized_keys`。

## 测试

```bash
export CARGO_HOME="$PWD/.cargo-home"
cargo test
cargo clippy --all-targets -- -D warnings
./scripts/local-smoke.sh
./scripts/managed-smoke.sh
```

两个 smoke test 都只在 `.local-test/` 内生成一次性服务端、SSH 密钥、Astra known-hosts 和数据库。它们会验证首次 `accept-new`、后续严格主机校验以及显式证书 pin。Managed 测试还会验证目标 UID、跨账户拒绝、UTF-8 locale fallback、`TERM`/`IUTF8` 以及 gateway 重启后 PTY 恢复。

## MVP 边界

这还不是完整产品：

- managed 模式已经按 Unix 账户、supplementary groups、GID 和 UID 隔离，但尚未接入 PAM、账户锁定/过期策略；
- 尚未实现按来源地址的认证速率限制、连接配额和审计日志后端，公开暴露前仍需补齐并接受独立安全审计；
- 认证兼容 OpenSSH Ed25519 密钥格式和 `authorized_keys`，客户端会自动选择 `~/.ssh/id_ed25519`，但暂不支持 ssh-agent、加密私钥、其他密钥算法、SSH 用户证书及 authorized_keys options；
- QUIC 主机身份已经支持独立的 SSH 式 TOFU 文件，但当前 pin 的是完整自签名证书；正式的证书轮换机制尚未实现；
- 保存的是有界原始输出，不是语义 screen/grid 快照；
- 暂无 QUIC DATAGRAM 累计状态同步、预测、文件和端口通道；
- 暂无 SSH stdio fallback；
- rootless 模式的 PTY 仍由 gateway 进程持有；managed 模式已经使用可跨 gateway 重启存活的独立用户 worker，但尚未提供正式的 worker 停止/升级管理命令。

线协议定义见 [`proto/astra.proto`](proto/astra.proto)，总体产品方向见 [`quic-mosh-tmux-implementation-plan.md`](quic-mosh-tmux-implementation-plan.md)。
