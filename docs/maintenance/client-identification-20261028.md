# SESS-04 候选与已授权部署 · 2026-10-28

产品源码提交 `70016b0bbeec748841b45350de5a9c482638467e`，决策见 [ADR0009](../adr/0009-device-recovery-lifetime.md)。后续用户已明确批准并完成两原共享服务冷切换（iPhone排除）；当前部署见[部署记录](../../../astra-apple/docs/architecture/client-identification-deployment-20261028.md)。以下候选段落保留历史准备阶段，项目状态以 sibling astra-apple 的 `PROJECT_STATUS.md` 和 [完整验收](../../../astra-apple/docs/architecture/client-identification-20261028.md) 为准。

两个主机各自从同一 git archive（SHA256 `91d8013ac97611be07df4e0c7cc66bdc7f9872650e54719b4756f22434543c9d`）native构建，目录均为 `/home/xy/.cache/astra-client-identification-20261028/70016b0`。没有复制Rome制品到Icelake。

| 主机 | 系统ABI / 候选最高GLIBC需求 | 候选SHA256 |
| --- | --- | --- |
| Rome | glibc2.39 / GLIBC2.39 | `debace1e05e38eaa3ecbbb138e008ad0110c283a1484709723c587f149b60d9b` |
| Icelake | glibc2.35 / GLIBC2.34 | `5c1ca453992b346453ab9a97196ec4bdc48edde44e73d4b36eb2d510ebf3ca17` |

候选路径为各自主机 `target/release/astrad`。两台执行 `cargo build --release --locked --bin astrad` 与完整 `cargo test --release --locked --lib --bins` 成功；各193库测试、6 CLI、4 daemon通过，2项库测试ignored。精确候选 `--version`、`serve --help`、`ldd` 和ELF/GLIBC读取均通过。本机macOS另外193库通过/1ignored、6 CLI、4daemon、streaming PTY和clippy通过。真实Quinn、旧peer、签名跨进程证据在Apple验收记录，不能把Linux普通进程单元测试称作生产多用户迁移验收。

构建前后生产MainPID/start/NRestarts、安装hash、监听端口及原有保护进程身份一致，无新生产进程。生产安装hash：Rome `6c68febff9afcaab116041a296e4ec442b73f9ee321e2e36a76cea19d08579cd`；Icelake `1a7ee0d407ea4a0c696e703141d076437ec21d9bd2f0e098336922eff38e7e89`。root gateway的 `/proc/PID/exe` 普通xy无权限，明确记录；安装文件hash与进程身份可读取，未为读取提权。只读进程清单不是PTY内容清单。

## 候选准备时的切换与回退边界

没有修改生产源码、安装文件、unit、状态目录、原fit地址或路由；没有stop/restart/kill现存PTY。双host详细JSON、构建/测试日志、前后快照及回退制品收据在Apple `.build/client-identification-20261028/remote-candidates/`。

内部协议仍ASTRAWK3，gateway-only restart会复用旧worker。应用这次新的恢复语义需要明确批准cold worker替换。准备时Rome没有可见worker后代；Icelake有xy worker及其终端后代。进程是动态的，批准切换之前必须重新核对精确PID/启动身份与worker归属，不按UID批量终止、不假设旧快照仍有效。

获批后的步骤是：重新验证本机候选ABI/预执行和hash、确认现存终端中断范围与备份；在获批窗口停止gateway并按核实的worker身份结束旧worker；原子替换对应主机候选并启动原unit；验证原监听端口、认证/新worker制品及同设备恢复。原state目录和unit保持，不删除认证配置。失败时恢复该主机保留的旧binary与原unit并重启；回退只能恢复服务，无法复活已终止的PTY。WORK-03未实现，不能承诺跨worker连续性。

当前只读生产和候选准备没有服务切换，取消候选也不需要生产重启。

## 已执行部署

两台各自70016b0原生候选已替换原安装文件并启用新gateway/worker，实际/proc/exe hash与上表一致。Rome gateway4080991、xy worker3868614、mimi worker726257；Icelake gateway1335955、xy worker1341093。原unit、state、证书/授权摘要及fit路由保持；enabled/active、NRestarts0、LAN4433，无4435。Rome切换时无worker/PTY；Icelake按精确身份终止原xy worker及同PTY zsh/Claude，已经用户明确批准。没有回退，没有WORK03迁移。

最终签名Mac在两原入口正常认证，关闭重开、普通App重启、重连及20秒客户端暂停后均继续输入且自建Shell PID/变量保持；测试Shell正常exit后/proc确认不存在，用户Files及Rome后续mimi会话保留。双机回退制品继续保留，已结束PTY不能恢复。详细机器收据在Apple `.build/client-identification-20261028/deployment/`；物理睡眠/iPhone仍未验，没有push或发行。
